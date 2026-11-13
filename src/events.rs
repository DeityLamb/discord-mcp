//! What the gateway said, kept for whoever asks: a bounded log of events,
//! each with a cursor, read by any number of sessions without one taking
//! events from another.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use crate::filter::Program;
use crate::lock;

/// Events kept; the oldest go first.
pub const MAX_EVENTS: usize = 5_000;
/// Bytes of event JSON kept.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// What a server sends about itself on connect, by the hundreds or thousands. The count stays; REST has the rest.
const GUILD_LISTS: [&str; 11] = [
    "members",
    "presences",
    "channels",
    "threads",
    "roles",
    "emojis",
    "stickers",
    "voice_states",
    "guild_scheduled_events",
    "stage_instances",
    "soundboard_sounds",
];

pub struct Event {
    /// Its cursor: 1 for the first event, growing by one.
    pub seq: u64,
    /// The gateway's name for it: `MESSAGE_CREATE`.
    pub name: String,
    /// When it arrived, in seconds since the epoch.
    pub ts: u64,
    pub data: Value,
    bytes: usize,
}

pub struct EventLog {
    inner: Mutex<Inner>,
    changed: Notify,
    max_events: usize,
    max_bytes: usize,
}

#[derive(Default)]
struct Inner {
    events: VecDeque<Arc<Event>>,
    /// The cursor of the newest event ever logged.
    last: u64,
    bytes: usize,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::with_limits(MAX_EVENTS, MAX_BYTES)
    }
}

impl EventLog {
    pub fn with_limits(max_events: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::default(),
            changed: Notify::new(),
            max_events,
            max_bytes,
        }
    }

    /// Log an event as the gateway sent it; `bytes` is the size of its JSON.
    pub fn push(&self, name: &str, mut data: Value, bytes: usize) {
        let bytes = trim(name, &mut data).unwrap_or(bytes);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let mut inner = lock(&self.inner);
            inner.last += 1;
            let event = Event {
                seq: inner.last,
                name: name.to_owned(),
                ts,
                data,
                bytes,
            };
            inner.bytes += bytes;
            inner.events.push_back(Arc::new(event));
            while inner.events.len() > self.max_events || (inner.bytes > self.max_bytes && inner.events.len() > 1) {
                if let Some(dropped) = inner.events.pop_front() {
                    inner.bytes -= dropped.bytes;
                }
            }
        }
        self.changed.notify_waiters();
    }

    /// The cursor of the newest event: pass it as `after` to see only what comes later.
    pub fn cursor(&self) -> u64 {
        lock(&self.inner).last
    }

    /// Events after the cursor, oldest first, and how many after it are gone from the log.
    pub fn since(&self, after: u64) -> (Vec<Arc<Event>>, u64) {
        let inner = lock(&self.inner);
        let oldest = inner.events.front().map_or(inner.last + 1, |e| e.seq);
        let missed = oldest.saturating_sub(after + 1);
        let events = inner.events.iter().filter(|e| e.seq > after).cloned().collect();
        (events, missed)
    }

    /// Resolves at the next [`push`](Self::push) made after this is called and enabled.
    pub fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }
}

/// Cut an event down to what is worth keeping. Returns its new size if it was cut.
fn trim(name: &str, data: &mut Value) -> Option<usize> {
    match name {
        // A server's full state: thousands of members and channels REST lists on request.
        "GUILD_CREATE" => {
            let object = data.as_object_mut()?;
            let mut cut = false;
            for key in GUILD_LISTS {
                if let Some(Value::Array(items)) = object.get(key) {
                    let count = items.len();
                    object.remove(key);
                    object.insert(format!("{key}_count"), json!(count));
                    cut = true;
                }
            }
            cut.then(|| data.to_string().len())
        }
        // The session's keys stay with the connection; the log gets who and where.
        "READY" => {
            let mut kept = Map::new();
            for key in ["user", "application", "shard"] {
                if let Some(value) = data.get(key) {
                    kept.insert(key.to_owned(), value.clone());
                }
            }
            let guilds: Vec<Value> = data["guilds"]
                .as_array()
                .map(|g| g.iter().map(|guild| guild["id"].clone()).collect())
                .unwrap_or_default();
            kept.insert("guild_ids".to_owned(), Value::Array(guilds));
            *data = Value::Object(kept);
            Some(data.to_string().len())
        }
        _ => None,
    }
}

/// Which events a reader wants and how to show them.
pub struct Query {
    /// Event names, upper case. Empty: all.
    pub types: Vec<String>,
    /// A jq condition on the event's data.
    pub condition: Option<String>,
    /// A jq filter that shapes the event's data for showing.
    pub shape: Option<String>,
    /// Most events shown.
    pub limit: usize,
    /// Most characters shown.
    pub max_chars: usize,
}

pub struct Selection {
    /// One line per event: `#cursor NAME data`.
    pub lines: Vec<String>,
    /// The cursor to continue from: the last event looked at.
    pub cursor: u64,
    /// Matching events were left for the next call.
    pub more: bool,
}

/// Pick the events a query wants out of `events` (oldest first). Runs jq, so call it off the async threads.
pub fn select(events: &[Arc<Event>], after: u64, query: &Query) -> Result<Selection, String> {
    let condition = query.condition.as_deref().map(Program::compile).transpose()?;
    let shape = query.shape.as_deref().map(Program::compile).transpose()?;
    let mut selection = Selection {
        lines: Vec::new(),
        cursor: after,
        more: false,
    };
    let mut chars = 0;
    for event in events {
        let wanted = (query.types.is_empty() || query.types.iter().any(|t| *t == event.name))
            && condition.as_ref().is_none_or(|c| c.holds(&event.data));
        if !wanted {
            selection.cursor = event.seq;
            continue;
        }
        if selection.lines.len() >= query.limit || chars >= query.max_chars {
            selection.more = true;
            break;
        }
        let shown = match &shape {
            Some(shape) => shape
                .run(&event.data, query.max_chars)
                .unwrap_or_else(|e| format!("({e})")),
            None => event.data.to_string(),
        };
        let line = format!("#{} {} {shown}", event.seq, event.name);
        chars += line.len();
        selection.lines.push(line);
        selection.cursor = event.seq;
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> Query {
        Query {
            types: Vec::new(),
            condition: None,
            shape: None,
            limit: 50,
            max_chars: 10_000,
        }
    }

    fn message(log: &EventLog, channel: &str, content: &str) {
        let data = json!({"channel_id": channel, "content": content, "author": {"username": "ann"}});
        let bytes = data.to_string().len();
        log.push("MESSAGE_CREATE", data, bytes);
    }

    #[test]
    fn readers_page_with_cursors_and_never_take_from_each_other() {
        let log = EventLog::default();
        assert_eq!(log.cursor(), 0);
        message(&log, "1", "a");
        message(&log, "1", "b");
        let (first, missed) = log.since(0);
        assert_eq!((first.len(), missed), (2, 0));
        // A second reader sees the same events.
        assert_eq!(log.since(0).0.len(), 2);
        message(&log, "1", "c");
        let (later, _) = log.since(2);
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].data["content"], "c");
        assert_eq!(log.cursor(), 3);
    }

    #[test]
    fn the_oldest_events_go_and_a_reader_learns_how_many() {
        let log = EventLog::with_limits(3, MAX_BYTES);
        for i in 0..5 {
            message(&log, "1", &i.to_string());
        }
        let (events, missed) = log.since(0);
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(missed, 2);
        assert_eq!(log.since(3).1, 0);
        let by_size = EventLog::with_limits(100, 150);
        for i in 0..5 {
            message(&by_size, "1", &i.to_string());
        }
        assert!(by_size.since(0).0.len() < 5);
    }

    #[test]
    fn a_server_arriving_keeps_its_counts_not_its_lists() {
        let log = EventLog::default();
        let data = json!({"id": "9", "name": "home", "members": [{}, {}], "channels": [{}], "member_count": 2});
        log.push("GUILD_CREATE", data, 1_000);
        let (events, _) = log.since(0);
        assert_eq!(
            events[0].data,
            json!({"id": "9", "name": "home", "member_count": 2, "members_count": 2, "channels_count": 1})
        );
    }

    #[test]
    fn ready_keeps_no_session_keys() {
        let log = EventLog::default();
        let data = json!({"session_id": "s", "resume_gateway_url": "wss://x", "user": {"username": "bot"},
            "guilds": [{"id": "1", "unavailable": true}]});
        log.push("READY", data, 100);
        let (events, _) = log.since(0);
        assert_eq!(events[0].data, json!({"user": {"username": "bot"}, "guild_ids": ["1"]}));
    }

    #[test]
    fn selection_filters_shapes_and_limits() {
        let log = EventLog::default();
        message(&log, "1", "one");
        message(&log, "2", "two");
        log.push("TYPING_START", json!({"channel_id": "1"}), 20);
        message(&log, "1", "three");
        let (events, _) = log.since(0);

        let all = select(&events, 0, &query()).unwrap();
        assert_eq!(all.lines.len(), 4);
        assert_eq!(all.cursor, 4);
        assert!(all.lines[0].starts_with("#1 MESSAGE_CREATE {"), "{}", all.lines[0]);

        let wanted = Query {
            types: vec!["MESSAGE_CREATE".into()],
            condition: Some(".channel_id == \"1\"".into()),
            shape: Some("\"\\(.author.username): \\(.content)\"".into()),
            ..query()
        };
        let picked = select(&events, 0, &wanted).unwrap();
        assert_eq!(
            picked.lines,
            ["#1 MESSAGE_CREATE ann: one", "#4 MESSAGE_CREATE ann: three"]
        );
        assert_eq!(picked.cursor, 4);
        assert!(!picked.more);

        let one_at_a_time = Query { limit: 1, ..query() };
        let page = select(&events, 0, &one_at_a_time).unwrap();
        assert_eq!((page.lines.len(), page.cursor, page.more), (1, 1, true));

        // Events that don't match still move the cursor: the next call starts after them.
        let none = Query {
            types: vec!["GUILD_BAN_ADD".into()],
            ..query()
        };
        let nothing = select(&events, 0, &none).unwrap();
        assert_eq!((nothing.lines.len(), nothing.cursor), (0, 4));

        assert!(
            select(
                &events,
                0,
                &Query {
                    condition: Some(".[".into()),
                    ..query()
                }
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_waiter_wakes_on_the_next_event() {
        let log = Arc::new(EventLog::default());
        let waiting = {
            let log = log.clone();
            tokio::spawn(async move {
                let changed = log.changed();
                tokio::pin!(changed);
                changed.as_mut().enable();
                changed.await;
                log.cursor()
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        message(&log, "1", "wake up");
        assert_eq!(waiting.await.unwrap(), 1);
    }
}
