//! The gateway: one websocket session with Discord, whose events go into
//! the log. It starts when someone first asks for events, so a session that
//! only uses REST never opens it (and the bot stays offline).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::Method;
use serde_json::Value;
use tokio::sync::Notify;
use twilight_gateway::{CloseFrame, ConfigBuilder, Intents, Message, Shard, ShardId};

use crate::events::EventLog;
use crate::rest::{Body, Layout, Request, Rest};

/// How soon after Discord closed the session for good a new one may be tried: each try spends one
/// of the bot's 1000 daily identifies.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Longest the first reader waits for the session to come up.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest it then waits for the servers to arrive, so its cursor starts after them.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
/// Time Discord gets to answer a goodbye.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Every intent by the name `--intents` takes, with its bit.
pub const INTENTS: [(&str, u64); 21] = [
    ("guilds", 1 << 0),
    ("guild_members", 1 << 1),
    ("guild_moderation", 1 << 2),
    ("guild_expressions", 1 << 3),
    ("guild_integrations", 1 << 4),
    ("guild_webhooks", 1 << 5),
    ("guild_invites", 1 << 6),
    ("guild_voice_states", 1 << 7),
    ("guild_presences", 1 << 8),
    ("guild_messages", 1 << 9),
    ("guild_message_reactions", 1 << 10),
    ("guild_message_typing", 1 << 11),
    ("direct_messages", 1 << 12),
    ("direct_message_reactions", 1 << 13),
    ("direct_message_typing", 1 << 14),
    ("message_content", 1 << 15),
    ("guild_scheduled_events", 1 << 16),
    ("auto_moderation_configuration", 1 << 20),
    ("auto_moderation_execution", 1 << 21),
    ("guild_message_polls", 1 << 24),
    ("direct_message_polls", 1 << 25),
];

const GUILD_MEMBERS: u64 = 1 << 1;
const GUILD_PRESENCES: u64 = 1 << 8;
const MESSAGE_CONTENT: u64 = 1 << 15;
/// Typing events are many and say little.
const TYPING: u64 = 1 << 11 | 1 << 14;
/// The intents Discord only gives a bot that has them switched on in the Developer Portal.
const PRIVILEGED: u64 = GUILD_MEMBERS | GUILD_PRESENCES | MESSAGE_CONTENT;

/// An application's flags that say a privileged intent is switched on (in full, or for a small bot).
const FLAG_GUILD_MEMBERS: u64 = 1 << 14 | 1 << 15;
const FLAG_MESSAGE_CONTENT: u64 = 1 << 18 | 1 << 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentChoice {
    /// Everything but typing and presences, with the privileged intents the bot has switched on.
    Auto,
    Fixed(u64),
}

/// The intents asked for when nothing says otherwise and nothing is known about the bot.
pub fn default_intents() -> u64 {
    INTENTS.iter().map(|(_, bit)| bit).sum::<u64>() & !PRIVILEGED & !TYPING
}

/// `--intents`: names separated by commas (`guilds,guild_messages,message_content`), or the number.
pub fn parse_intents(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if let Ok(bits) = text.parse::<u64>() {
        return Ok(bits);
    }
    let mut bits = 0;
    for name in text.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let wanted = name.to_ascii_lowercase().replace('-', "_");
        match INTENTS.iter().find(|(known, _)| *known == wanted) {
            Some((_, bit)) => bits |= bit,
            None => {
                let names: Vec<&str> = INTENTS.iter().map(|(n, _)| *n).collect();
                return Err(format!("no intent is named {name}; there are {}", names.join(", ")));
            }
        }
    }
    if bits == 0 {
        return Err("no intents given".into());
    }
    Ok(bits)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No session yet: nobody has asked for events.
    Idle,
    Connecting,
    Ready {
        user: String,
        guilds: usize,
    },
    /// The connection dropped; a new one is being made.
    Reconnecting {
        why: String,
    },
    /// Discord ended the session for good, or it couldn't be set up.
    Closed {
        why: String,
        at: Instant,
    },
}

impl State {
    pub fn describe(&self) -> String {
        match self {
            Self::Idle => "not connected".into(),
            Self::Connecting => "connecting".into(),
            Self::Ready { user, guilds } => {
                format!(
                    "connected as {user}, in {guilds} server{}",
                    if *guilds == 1 { "" } else { "s" }
                )
            }
            Self::Reconnecting { why } => format!("reconnecting ({why})"),
            Self::Closed { why, .. } => format!("closed: {why}"),
        }
    }
}

pub struct Gateway {
    token: String,
    rest: Arc<Rest>,
    intents: IntentChoice,
    /// Connect here instead of to Discord: a stand-in gateway in tests.
    url: Option<String>,
    pub log: Arc<EventLog>,
    state: Mutex<State>,
    /// Who the session is, kept across reconnects.
    identity: Mutex<Option<(String, usize)>>,
    /// Servers that have arrived since the session came up.
    arrived: AtomicUsize,
    state_changed: Notify,
    stop: Notify,
}

impl Gateway {
    pub fn new(token: &str, rest: Arc<Rest>, intents: IntentChoice, url: Option<String>) -> Arc<Self> {
        let token = token.trim();
        Arc::new(Self {
            token: token.strip_prefix("Bot ").unwrap_or(token).trim().to_owned(),
            rest,
            intents,
            url,
            log: Arc::new(EventLog::default()),
            state: Mutex::new(State::Idle),
            identity: Mutex::new(None),
            arrived: AtomicUsize::new(0),
            state_changed: Notify::new(),
            stop: Notify::new(),
        })
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, state: State) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
        self.state_changed.notify_waiters();
    }

    /// Open the session if there is none. Says whether this call started it.
    pub fn start(self: &Arc<Self>) -> bool {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*state {
                State::Idle => {}
                State::Closed { at, .. } if at.elapsed() >= RETRY_AFTER => {}
                _ => return false,
            }
            *state = State::Connecting;
        }
        self.arrived.store(0, Ordering::Relaxed);
        let gateway = self.clone();
        tokio::spawn(async move {
            let session = tokio::spawn(gateway.clone().run());
            if let Err(crash) = session.await {
                gateway.set(State::Closed {
                    why: format!("the gateway task stopped unexpectedly ({crash})"),
                    at: Instant::now(),
                });
            }
        });
        true
    }

    /// Wait for a session just started to come up and for its servers to arrive, so a cursor taken
    /// afterwards points past them.
    pub async fn settle(&self) {
        let ready_by = Instant::now() + READY_TIMEOUT;
        loop {
            let changed = self.state_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.state() {
                State::Connecting | State::Reconnecting { .. } if Instant::now() < ready_by => {}
                State::Ready { guilds, .. } => {
                    let settled_by = Instant::now() + SETTLE_TIMEOUT;
                    while self.arrived.load(Ordering::Relaxed) < guilds && Instant::now() < settled_by {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    return;
                }
                _ => return,
            }
            let _ = tokio::time::timeout_at(ready_by.into(), changed).await;
        }
    }

    /// Say goodbye to Discord, so the bot goes offline at once rather than when the session times out.
    pub async fn stop(&self) {
        if matches!(self.state(), State::Idle | State::Closed { .. }) {
            return;
        }
        self.stop.notify_one();
        let done_by = Instant::now() + CLOSE_TIMEOUT + Duration::from_secs(1);
        while !matches!(self.state(), State::Idle | State::Closed { .. }) && Instant::now() < done_by {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The intents to identify with.
    async fn intents(&self) -> Result<u64, String> {
        if let IntentChoice::Fixed(bits) = self.intents {
            return Ok(bits);
        }
        // The application's flags say which privileged intents are switched on; asking for one that
        // isn't gets the session refused.
        let request = Request {
            method: Method::GET,
            path: "/applications/@me".into(),
            bucket: "GET/applications/@me".into(),
            query: Vec::new(),
            body: None,
            files: Vec::new(),
            layout: Layout::PayloadJson,
            reason: None,
        };
        let mut bits = default_intents();
        match self.rest.send(&request).await {
            Ok(response) if response.status.as_u16() == 401 => {
                return Err("Discord rejected the bot token (check DISCORD_TOKEN)".into());
            }
            Ok(response) => match response.body {
                Body::Json(application) if response.status.is_success() => {
                    let flags = application["flags"].as_u64().unwrap_or(0);
                    if flags & FLAG_GUILD_MEMBERS != 0 {
                        bits |= GUILD_MEMBERS;
                    }
                    if flags & FLAG_MESSAGE_CONTENT != 0 {
                        bits |= MESSAGE_CONTENT;
                    }
                }
                _ => tracing::warn!(
                    "couldn't read the application's flags ({}); connecting without privileged intents",
                    response.status
                ),
            },
            Err(e) => {
                tracing::warn!("couldn't read the application's flags ({e}); connecting without privileged intents")
            }
        }
        Ok(bits)
    }

    async fn run(self: Arc<Self>) {
        let intents = match self.intents().await {
            Ok(bits) => bits,
            Err(why) => {
                return self.set(State::Closed {
                    why,
                    at: Instant::now(),
                });
            }
        };
        let mut config = ConfigBuilder::new(self.token.clone(), Intents::from_bits_retain(intents));
        if let Some(url) = &self.url {
            config = config.proxy_url(url.clone());
        }
        let mut shard = Shard::with_config(ShardId::ONE, config.build());
        let mut last_close = None;
        loop {
            let next = tokio::select! {
                next = shard.next() => next,
                () = self.stop.notified() => {
                    shard.close(CloseFrame::NORMAL);
                    // Keep reading until the goodbye is out and answered.
                    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
                        while let Some(next) = shard.next().await {
                            if matches!(next, Ok(Message::Close(_))) {
                                break;
                            }
                        }
                    })
                    .await;
                    return self.set(State::Idle);
                }
            };
            match next {
                Some(Ok(Message::Text(json))) => self.dispatch(&json),
                Some(Ok(Message::Close(frame))) => {
                    last_close = frame.as_ref().map(|f| f.code);
                    self.set(State::Reconnecting {
                        why: match last_close {
                            Some(code) => format!("Discord closed the connection with code {code}"),
                            None => "the connection dropped".into(),
                        },
                    });
                }
                // Connecting failed; the shard waits longer each time before the next try.
                Some(Err(error)) => {
                    tracing::warn!("gateway: {error}");
                    self.set(State::Reconnecting {
                        why: "can't reach the gateway".into(),
                    });
                }
                None => {
                    return self.set(State::Closed {
                        why: closed_for_good(last_close),
                        at: Instant::now(),
                    });
                }
            }
        }
    }

    /// One payload from the gateway. Only dispatches (opcode 0) are events; the shard itself answers the rest.
    fn dispatch(&self, json: &str) {
        let Ok(mut payload) = serde_json::from_str::<Value>(json) else {
            return;
        };
        if payload["op"] != 0 {
            return;
        }
        let Some(name) = payload["t"].as_str().map(str::to_owned) else {
            return;
        };
        let data = payload["d"].take();
        match name.as_str() {
            "READY" => {
                let user = data["user"]["username"].as_str().unwrap_or("the bot").to_owned();
                let guilds = data["guilds"].as_array().map_or(0, Vec::len);
                *self.identity.lock().unwrap_or_else(|e| e.into_inner()) = Some((user.clone(), guilds));
                self.arrived.store(0, Ordering::Relaxed);
                self.set(State::Ready { user, guilds });
            }
            "RESUMED" => {
                let identity = self.identity.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let (user, guilds) = identity.unwrap_or_else(|| ("the bot".into(), 0));
                self.set(State::Ready { user, guilds });
            }
            "GUILD_CREATE" => {
                self.arrived.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.log.push(&name, data, json.len());
    }
}

/// Why Discord won't take this session back, and what would change that.
fn closed_for_good(code: Option<u16>) -> String {
    match code {
        Some(4004) => "Discord rejected the bot token (check DISCORD_TOKEN)".into(),
        Some(4011) => "the bot is in too many servers for one connection, and sharding isn't supported".into(),
        Some(4013) => "Discord didn't accept the intents value (check --intents)".into(),
        Some(4014) => "Discord refused an intent that isn't switched on for this bot: switch on Message Content \
                       or Server Members under Privileged Gateway Intents on the application's Bot page in the \
                       Developer Portal, or leave it out of --intents"
            .into(),
        Some(code) => format!("Discord closed the session with code {code}"),
        None => "Discord closed the session".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_intents_leave_out_typing_and_the_privileged_ones() {
        let bits = default_intents();
        assert_eq!(bits & PRIVILEGED, 0);
        assert_eq!(bits & TYPING, 0);
        assert_ne!(bits & (1 << 9), 0, "guild messages");
        assert_ne!(bits & (1 << 12), 0, "direct messages");
    }

    #[test]
    fn intents_parse_from_names_or_a_number() {
        assert_eq!(
            parse_intents("guilds, guild-messages,MESSAGE_CONTENT").unwrap(),
            1 | 1 << 9 | 1 << 15
        );
        assert_eq!(parse_intents("513").unwrap(), 513);
        assert!(
            parse_intents("guilds,nope")
                .unwrap_err()
                .contains("no intent is named nope")
        );
        assert!(parse_intents("").is_err());
    }

    #[test]
    fn states_read_as_sentences() {
        let ready = State::Ready {
            user: "helper".into(),
            guilds: 1,
        };
        assert_eq!(ready.describe(), "connected as helper, in 1 server");
        assert!(closed_for_good(Some(4014)).contains("Privileged Gateway Intents"));
    }
}
