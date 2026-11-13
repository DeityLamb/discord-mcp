//! Several sessions at once: real server processes against a stand-in for
//! Discord (REST and gateway). One hub, one gateway connection, one event
//! log that every session reads without taking from the others.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::broadcast;

/// Flags of an application with the Message Content intent switched on.
const MESSAGE_CONTENT_FLAG: u64 = 1 << 19;

struct Discord {
    /// Sessions the gateway has seen identify.
    identifies: AtomicUsize,
    /// The intents of the last identify.
    intents: Mutex<Option<u64>>,
    /// Close codes clients left with.
    closes: Mutex<Vec<u16>>,
    /// Events to send to every connected client.
    events: broadcast::Sender<Value>,
    rest_hits: AtomicUsize,
    port: u16,
}

async fn rest(State(discord): State<Arc<Discord>>, request: Request) -> Response {
    discord.rest_hits.fetch_add(1, Ordering::SeqCst);
    match (request.method().as_str(), request.uri().path()) {
        ("GET", "/applications/@me") => axum::Json(json!({"id": "99", "flags": MESSAGE_CONTENT_FLAG})).into_response(),
        ("GET", "/users/@me") => axum::Json(json!({"id": "42", "username": "helper"})).into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"message": "404: Not Found", "code": 0})),
        )
            .into_response(),
    }
}

async fn gateway(State(discord): State<Arc<Discord>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| session(socket, discord))
}

async fn session(mut socket: WebSocket, discord: Arc<Discord>) {
    let mut events = discord.events.subscribe();
    let mut seq = 0;
    let text = |value: Value| Message::Text(value.to_string().into());
    let hello = json!({"op": 10, "d": {"heartbeat_interval": 45_000}});
    if socket.send(text(hello)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            received = socket.recv() => match received {
                Some(Ok(Message::Text(payload))) => {
                    let payload: Value = serde_json::from_str(&payload).unwrap();
                    match payload["op"].as_u64() {
                        // Identify: the session is up, and the bot's one server arrives.
                        Some(2) => {
                            discord.identifies.fetch_add(1, Ordering::SeqCst);
                            *discord.intents.lock().unwrap() = payload["d"]["intents"].as_u64();
                            let ready = json!({"op": 0, "t": "READY", "s": seq + 1, "d": {
                                "v": 10,
                                "user": {"id": "42", "username": "helper"},
                                "guilds": [{"id": "1", "unavailable": true}],
                                "session_id": "session",
                                "resume_gateway_url": format!("ws://127.0.0.1:{}", discord.port),
                                "application": {"id": "99", "flags": 0},
                            }});
                            let guild = json!({"op": 0, "t": "GUILD_CREATE", "s": seq + 2, "d": {
                                "id": "1", "name": "home", "member_count": 3, "members": [{}, {}, {}], "channels": [{}],
                            }});
                            seq += 2;
                            if socket.send(text(ready)).await.is_err() || socket.send(text(guild)).await.is_err() {
                                return;
                            }
                        }
                        Some(1) => {
                            let _ = socket.send(text(json!({"op": 11}))).await;
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    discord.closes.lock().unwrap().push(frame.map_or(0, |f| f.code));
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
            event = events.recv() => {
                let Ok(event) = event else { return };
                seq += 1;
                let dispatch = json!({"op": 0, "t": event["t"], "s": seq, "d": event["d"]});
                if socket.send(text(dispatch)).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn discord() -> Arc<Discord> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let discord = Arc::new(Discord {
        identifies: AtomicUsize::new(0),
        intents: Mutex::new(None),
        closes: Mutex::new(Vec::new()),
        events: broadcast::channel(64).0,
        rest_hits: AtomicUsize::new(0),
        port,
    });
    // The gateway is the websocket at the root; everything else is REST.
    let app = Router::new()
        .route("/", get(gateway))
        .fallback(rest)
        .with_state(discord.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    discord
}

/// One MCP client with its own server process.
struct Client {
    _child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Client {
    async fn start(discord: &Discord, state: &Path, flags: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_discord-mcp"))
            .arg("--no-spec-update")
            .args(flags)
            .env("DISCORD_TOKEN", "tok")
            .env("DISCORD_API_BASE", format!("http://127.0.0.1:{}", discord.port))
            .env("DISCORD_GATEWAY_URL", format!("ws://127.0.0.1:{}", discord.port))
            .env("DISCORD_MCP_STATE_DIR", state)
            .env("DISCORD_MCP_HUB_GRACE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut client = Self {
            stdin: child.stdin.take(),
            lines: BufReader::new(child.stdout.take().unwrap()).lines(),
            _child: child,
            next_id: 0,
        };
        let hello = client
            .request(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
            )
            .await;
        assert_eq!(hello["result"]["serverInfo"]["name"], "discord-mcp", "{hello}");
        client
            .write(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        client
    }

    async fn write(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("the session is open");
        stdin.write_all(format!("{message}\n").as_bytes()).await.unwrap();
    }

    /// Send a request without waiting for its answer.
    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    async fn answer(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
                .await
                .expect("the server answers in time")
                .unwrap()
                .expect("the server is still there");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.answer(id).await
    }

    async fn tool_names(&mut self) -> Vec<String> {
        let listed = self.request("tools/list", json!({})).await;
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Call a tool; the text of its answer and whether it failed.
    async fn tool(&mut self, name: &str, arguments: Value) -> (bool, String) {
        let id = self
            .send("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        self.tool_answer(id).await
    }

    async fn tool_answer(&mut self, id: u64) -> (bool, String) {
        let answer = self.answer(id).await;
        let result = &answer["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        (result["isError"] == true, text)
    }

    /// Close stdin, as a client does when it is done.
    fn leave(&mut self) {
        self.stdin = None;
    }
}

fn cursor_of(text: &str) -> u64 {
    let line = text.lines().find(|l| l.starts_with("cursor: ")).expect("a cursor line");
    line["cursor: ".len()..]
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn hub_files(state: &Path) -> Vec<String> {
    std::fs::read_dir(state)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("hub-") && name.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default()
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn sessions_share_one_hub_one_connection_and_every_event() {
    let discord = discord().await;
    let state = tempfile::tempdir().unwrap();

    // Three clients start at the same moment: one of them wins the race to be the hub's parent.
    let (mut a, mut b, mut c) = tokio::join!(
        Client::start(&discord, state.path(), &[]),
        Client::start(&discord, state.path(), &[]),
        Client::start(&discord, state.path(), &["--read-only"]),
    );
    assert_eq!(hub_files(state.path()).len(), 1, "one hub for the three of them");
    assert_eq!(
        a.tool_names().await,
        ["discord_docs", "discord_get", "discord_call", "discord_events"]
    );
    // Each session keeps its own options, though the hub is shared.
    assert_eq!(c.tool_names().await, ["discord_docs", "discord_get", "discord_events"]);

    // REST goes through the hub.
    let (failed, me) = a
        .tool("discord_get", json!({"path": "/users/@me", "jq": ".username"}))
        .await;
    assert!(!failed, "{me}");
    assert_eq!(me, "helper");

    // Nobody has asked for events: no gateway session yet.
    assert_eq!(discord.identifies.load(Ordering::SeqCst), 0);

    // The first ask opens the session; the cursor it gets is past the server arriving.
    let (failed, first) = a.tool("discord_events", json!({})).await;
    assert!(!failed, "{first}");
    assert!(first.contains("Gateway: connected as helper, in 1 server."), "{first}");
    let after_start = cursor_of(&first);
    assert_eq!(after_start, 2, "{first}");
    assert_eq!(discord.identifies.load(Ordering::SeqCst), 1);
    // The bot has Message Content switched on, so it was asked for; presences and typing never are.
    let intents = discord.intents.lock().unwrap().unwrap();
    assert_ne!(intents & (1 << 15), 0, "message content");
    assert_eq!(intents & (1 << 8 | 1 << 1 | 1 << 11), 0, "presences, members, typing");

    // Another session reads what arrived before it ever asked, from the same buffer.
    let (failed, backlog) = b.tool("discord_events", json!({"after": 0})).await;
    assert!(!failed, "{backlog}");
    assert!(backlog.contains("#1 READY "), "{backlog}");
    assert!(backlog.contains("#2 GUILD_CREATE "), "{backlog}");
    // The server's lists are counts, and the session's keys aren't in the log.
    assert!(backlog.contains("\"members_count\":3"), "{backlog}");
    assert!(!backlog.contains("session_id"), "{backlog}");
    assert_eq!(discord.identifies.load(Ordering::SeqCst), 1, "still one connection");

    // A session waits for a message; it arrives while it waits.
    let waiting = c
        .send(
            "tools/call",
            json!({"name": "discord_events", "arguments": {
                "types": ["message_create"],
                "filter": ".channel_id == \"7\" and (.author.bot | not)",
                "jq": "\"\\(.author.username): \\(.content)\"",
                "wait": 20,
            }}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let message = |channel: &str, content: &str, bot: bool| {
        json!({"t": "MESSAGE_CREATE", "d": {
            "id": "500", "channel_id": channel, "content": content, "author": {"username": "ann", "bot": bot},
        }})
    };
    discord.events.send(message("8", "other channel", false)).unwrap();
    discord.events.send(message("7", "from a bot", true)).unwrap();
    discord.events.send(message("7", "hello there", false)).unwrap();
    let (failed, heard) = c.tool_answer(waiting).await;
    assert!(!failed, "{heard}");
    assert_eq!(
        heard.lines().next().unwrap(),
        "#5 MESSAGE_CREATE ann: hello there",
        "{heard}"
    );
    assert_eq!(cursor_of(&heard), 5);

    // The first session reads the same events from its own cursor: nothing was taken.
    let (_, also) = a
        .tool(
            "discord_events",
            json!({"after": after_start, "types": ["MESSAGE_CREATE"], "jq": ".content"}),
        )
        .await;
    let contents: Vec<&str> = also.lines().take(3).collect();
    assert_eq!(
        contents,
        [
            "#3 MESSAGE_CREATE other channel",
            "#4 MESSAGE_CREATE from a bot",
            "#5 MESSAGE_CREATE hello there"
        ],
        "{also}"
    );

    // Nothing new: an answer at once without wait, and after the wait with it.
    let (failed, quiet) = b.tool("discord_events", json!({})).await;
    assert!(!failed);
    assert!(quiet.starts_with("No matching events yet.\ncursor: 5"), "{quiet}");
    let started = Instant::now();
    let (_, waited) = b.tool("discord_events", json!({"wait": 1})).await;
    assert!(waited.starts_with("No matching events in the 1 s waited."), "{waited}");
    assert!(started.elapsed() >= Duration::from_secs(1));

    // A bad filter is said at once, not after the wait.
    let (failed, bad) = b.tool("discord_events", json!({"filter": ".[", "wait": 20})).await;
    assert!(failed);
    assert!(bad.contains("doesn't parse"), "{bad}");

    // Everyone leaves. The hub says goodbye to Discord and goes.
    a.leave();
    b.leave();
    c.leave();
    eventually("the hub leaves once nobody is attached", || {
        hub_files(state.path()).is_empty()
    })
    .await;
    eventually("the gateway session is closed properly", || {
        discord.closes.lock().unwrap().contains(&1000)
    })
    .await;

    // A later session starts a hub of its own, with a new connection when it asks for events.
    let mut later = Client::start(&discord, state.path(), &[]).await;
    assert_eq!(hub_files(state.path()).len(), 1);
    let (failed, again) = later.tool("discord_events", json!({"after": 99})).await;
    assert!(!failed, "{again}");
    assert!(again.contains("Cursor 99 is from an earlier connection"), "{again}");
    assert_eq!(discord.identifies.load(Ordering::SeqCst), 2);
    later.leave();
    eventually("the second hub leaves too", || hub_files(state.path()).is_empty()).await;
}

#[tokio::test]
async fn without_the_gateway_there_is_no_hub_and_no_events_tool() {
    let discord = discord().await;
    let state = tempfile::tempdir().unwrap();
    let mut client = Client::start(&discord, state.path(), &["--disable-gateway"]).await;
    assert_eq!(
        client.tool_names().await,
        ["discord_docs", "discord_get", "discord_call"]
    );
    let (failed, me) = client
        .tool("discord_get", json!({"path": "/users/@me", "jq": ".id"}))
        .await;
    assert!(!failed, "{me}");
    assert_eq!(me, "42");
    assert!(hub_files(state.path()).is_empty());
    assert_eq!(discord.identifies.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_session_can_keep_the_gateway_to_itself() {
    let discord = discord().await;
    let state = tempfile::tempdir().unwrap();
    let mut client = Client::start(
        &discord,
        state.path(),
        &["--no-hub", "--intents", "guilds,guild_messages"],
    )
    .await;
    assert!(hub_files(state.path()).is_empty());
    let (failed, first) = client.tool("discord_events", json!({"after": 0})).await;
    assert!(!failed, "{first}");
    assert!(first.contains("#1 READY "), "{first}");
    // The intents given are the intents asked for, with no look at the application's flags.
    assert_eq!(*discord.intents.lock().unwrap(), Some(1 | 1 << 9));
    assert_eq!(discord.rest_hits.load(Ordering::SeqCst), 0);
    client.leave();
    eventually("the session's own connection is closed properly", || {
        discord.closes.lock().unwrap().contains(&1000)
    })
    .await;
}
