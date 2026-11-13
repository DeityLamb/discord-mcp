//! The tools against a stand-in for Discord's API: what goes on the wire,
//! how rate limits and failures are handled, what an agent gets back.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use discord_mcp::rest::Rest;
use discord_mcp::tools::{CALL, GET, Options, Tools};
use discord_mcp::{spec, update};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<Seen>>,
    hits: Mutex<HashMap<String, usize>>,
}

#[derive(Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Mock {
    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a request reached the server")
    }

    fn hits(&self, key: &str) -> usize {
        self.hits.lock().unwrap().get(key).copied().unwrap_or(0)
    }
}

fn limits(bucket: &str, remaining: u32, reset_after: &str) -> [(&'static str, String); 3] {
    [
        ("x-ratelimit-bucket", bucket.to_owned()),
        ("x-ratelimit-remaining", remaining.to_string()),
        ("x-ratelimit-reset-after", reset_after.to_owned()),
    ]
}

async fn handle(State(mock): State<Arc<Mock>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap().to_vec();
    let key = format!("{} {}", parts.method, parts.uri.path());
    let hit = {
        let mut hits = mock.hits.lock().unwrap();
        let n = hits.entry(key.clone()).or_default();
        *n += 1;
        *n
    };
    mock.seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or("").to_owned(),
        headers: parts.headers.clone(),
        body: body.clone(),
    });
    let ok = |value: Value| (StatusCode::OK, axum::Json(value)).into_response();
    match (key.as_str(), hit) {
        ("GET /users/@me", _) => ok(json!({"id": "42", "username": "bot"})),
        ("GET /guilds/1/channels", _) => ok(json!([
            {"id": "10", "name": "general", "type": 0, "topic": "x"},
            {"id": "11", "name": "dev", "type": 0, "topic": "y"}
        ])),
        ("GET /channels/1/messages", 1) => (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"message": "You are being rate limited.", "retry_after": 0.2, "global": false})),
        )
            .into_response(),
        ("GET /channels/1/messages", _) => ok(json!([{"id": "100", "content": "hello"}])),
        ("GET /channels/2/messages", _) => (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"message": "You are being rate limited.", "retry_after": 60.0, "global": true})),
        )
            .into_response(),
        ("GET /channels/3/messages", _) => (limits("b3", 0, "0.3"), axum::Json(json!([]))).into_response(),
        ("POST /channels/1/messages", _) => {
            let echoed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
            ok(json!({"id": "200", "echo": echoed}))
        }
        ("POST /guilds/1/stickers", _) => ok(json!({"id": "300"})),
        ("POST /channels/1/typing", _) => StatusCode::NO_CONTENT.into_response(),
        ("DELETE /channels/1/messages/2", _) => StatusCode::NO_CONTENT.into_response(),
        ("PUT /channels/1/messages/2/reactions/%F0%9F%91%8D/@me", _) => StatusCode::NO_CONTENT.into_response(),
        ("GET /guilds/9/roles", _) => (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"message": "Missing Permissions", "code": 50013})),
        )
            .into_response(),
        ("GET /voice/regions", 1) => StatusCode::BAD_GATEWAY.into_response(),
        ("GET /voice/regions", _) => ok(json!([{"id": "rotterdam"}])),
        ("POST /channels/5/messages", _) => StatusCode::BAD_GATEWAY.into_response(),
        ("GET /brand-new/thing", _) => ok(json!({"new": true})),
        ("GET /guilds/1/members", _) => ok(json!(
            (0..500)
                .map(|i| json!({"user": {"id": i.to_string(), "username": format!("member{i}")}}))
                .collect::<Vec<_>>()
        )),
        ("GET /spec.json", _) => {
            if parts.headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("\"v1\"") {
                return StatusCode::NOT_MODIFIED.into_response();
            }
            // Discord's spec plus one endpoint it doesn't have yet.
            let mut doc: Value = serde_json::from_str(SPEC).unwrap();
            doc["paths"]["/fresh-endpoint"] = json!({"get": {"operationId": "get_fresh_endpoint", "responses": {}}});
            ([("etag", "\"v1\"")], axum::Json(doc)).into_response()
        }
        ("GET /not-a-spec.json", _) => ok(json!({"paths": {}})),
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"message": "404: Not Found", "code": 0})),
        )
            .into_response(),
    }
}

async fn discord() -> (Arc<Mock>, String) {
    let mock = Arc::new(Mock::default());
    let app = Router::new().fallback(handle).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (mock, base)
}

fn options() -> Options {
    Options {
        read_only: false,
        allow_unlisted: false,
        max_output: 40_000,
        cwd: PathBuf::from("/"),
    }
}

/// The spec in the repository: what the cache holds after a download.
const SPEC: &str = include_str!("../spec/openapi.json");

/// The reference in use, as after a first download. Every test shares it; a test may put a newer one in use.
fn reference() {
    if spec::current().is_none() {
        spec::replace(spec::Spec::parse(SPEC).unwrap());
    }
}

fn tools(base: &str, patience: Duration, options: Options) -> Tools {
    reference();
    Tools {
        rest: Some(Arc::new(Rest::new("tok", base, patience).unwrap())),
        gateway: None,
        options,
    }
}

async fn run(tools: &Tools, name: &str, args: Value) -> (bool, String) {
    let result: CallToolResult = tools.call(name, args.as_object().cloned()).await;
    let text = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<String>();
    (result.is_error.unwrap_or(false), text)
}

const PATIENT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_get_goes_out_as_the_bot_and_comes_back_as_json() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let (failed, text) = run(&tools, GET, json!({"path": "/users/@me"})).await;
    assert!(!failed, "{text}");
    assert_eq!(text, r#"{"id":"42","username":"bot"}"#);
    let seen = mock.last();
    assert_eq!(seen.header("authorization"), "Bot tok");
    assert!(
        seen.header("user-agent").starts_with("DiscordBot ("),
        "{}",
        seen.header("user-agent")
    );
}

#[tokio::test]
async fn query_parameters_and_jq() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let args = json!({"path": "/guilds/1/channels", "query": {"limit": 2}, "jq": ".[] | \"\\(.id) #\\(.name)\""});
    let (failed, text) = run(&tools, GET, args).await;
    assert!(!failed, "{text}");
    assert_eq!(text, "10 #general\n11 #dev");
    assert_eq!(mock.last().query, "limit=2");
    // A query string written into the path works the same.
    run(&tools, GET, json!({"path": "/guilds/1/channels?limit=3"})).await;
    assert_eq!(mock.last().query, "limit=3");
}

#[tokio::test]
async fn a_short_rate_limit_is_waited_out() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let started = Instant::now();
    let (failed, text) = run(&tools, GET, json!({"path": "/channels/1/messages"})).await;
    assert!(!failed, "{text}");
    assert!(text.contains("hello"), "{text}");
    assert_eq!(mock.hits("GET /channels/1/messages"), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_long_rate_limit_is_reported_and_then_respected() {
    let (mock, base) = discord().await;
    let tools = tools(&base, Duration::from_millis(300), options());
    let (failed, text) = run(&tools, GET, json!({"path": "/channels/2/messages"})).await;
    assert!(failed);
    assert!(
        text.contains("rate limiting the bot as a whole: try again in 60 s"),
        "{text}"
    );
    assert_eq!(mock.hits("GET /channels/2/messages"), 1);
    // The limit was global: nothing else goes out either, and Discord isn't asked again.
    let (failed, text) = run(&tools, GET, json!({"path": "/users/@me"})).await;
    assert!(failed, "{text}");
    assert_eq!(mock.hits("GET /users/@me"), 0);
}

#[tokio::test]
async fn a_spent_bucket_waits_instead_of_asking() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    run(&tools, GET, json!({"path": "/channels/3/messages"})).await;
    let started = Instant::now();
    let (failed, text) = run(&tools, GET, json!({"path": "/channels/3/messages"})).await;
    assert!(!failed, "{text}");
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(mock.hits("GET /channels/3/messages"), 2);
}

#[tokio::test]
async fn a_json_body_is_posted() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let args = json!({"method": "POST", "path": "/channels/1/messages", "body": {"content": "hi"}, "jq": ".id"});
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(!failed, "{text}");
    assert_eq!(text, "200");
    let seen = mock.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.header("content-type"), "application/json");
    assert_eq!(
        serde_json::from_slice::<Value>(&seen.body).unwrap(),
        json!({"content": "hi"})
    );
}

#[tokio::test]
async fn files_ride_along_with_the_body() {
    let (mock, base) = discord().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "the file's text").unwrap();
    let tools = tools(
        &base,
        PATIENT,
        Options {
            cwd: dir.path().to_owned(),
            ..options()
        },
    );
    let args = json!({
        "method": "POST",
        "path": "/channels/1/messages",
        "body": {"content": "see attached"},
        "files": [{"path": "notes.txt"}, {"path": dir.path().join("notes.txt"), "name": "renamed.md"}],
    });
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(!failed, "{text}");
    let seen = mock.last();
    assert!(
        seen.header("content-type").starts_with("multipart/form-data"),
        "{}",
        seen.header("content-type")
    );
    let wire = seen.text();
    assert!(wire.contains("name=\"payload_json\""), "{wire}");
    assert!(wire.contains("{\"content\":\"see attached\"}"), "{wire}");
    assert!(wire.contains("name=\"files[0]\"; filename=\"notes.txt\""), "{wire}");
    assert!(wire.contains("name=\"files[1]\"; filename=\"renamed.md\""), "{wire}");
    assert!(wire.contains("the file's text"), "{wire}");
    assert!(wire.to_lowercase().contains("content-type: text/plain"), "{wire}");
}

#[tokio::test]
async fn a_sticker_upload_uses_the_fields_its_endpoint_names() {
    let (mock, base) = discord().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.png"), b"\x89PNG").unwrap();
    let tools = tools(&base, PATIENT, options());
    let args = json!({
        "method": "POST",
        "path": "/guilds/1/stickers",
        "body": {"name": "wave", "tags": "hello", "description": "a wave"},
        "files": [{"path": dir.path().join("s.png")}],
    });
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(!failed, "{text}");
    let wire = mock.last().text();
    assert!(wire.contains("name=\"file\"; filename=\"s.png\""), "{wire}");
    assert!(wire.contains("name=\"name\""), "{wire}");
    assert!(!wire.contains("payload_json"), "{wire}");
    // An endpoint without uploads says so before anything is sent.
    let args =
        json!({"method": "DELETE", "path": "/channels/1/messages/2", "files": [{"path": dir.path().join("s.png")}]});
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(failed);
    assert!(text.contains("doesn't take file uploads"), "{text}");
    assert_eq!(mock.hits("DELETE /channels/1/messages/2"), 0);
}

#[tokio::test]
async fn a_post_without_a_body_still_has_a_length() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let (failed, text) = run(&tools, CALL, json!({"method": "POST", "path": "/channels/1/typing"})).await;
    assert!(!failed, "{text}");
    assert_eq!(text, "204 No Content: done, nothing to return.");
    assert_eq!(mock.last().header("content-length"), "0");
}

#[tokio::test]
async fn the_reason_reaches_the_audit_log_header() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let args = json!({"method": "DELETE", "path": "/channels/1/messages/2", "reason": "spam ✓"});
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(!failed, "{text}");
    assert_eq!(mock.last().header("x-audit-log-reason"), "spam %E2%9C%93");
}

#[tokio::test]
async fn an_emoji_in_the_path_is_encoded() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let args = json!({"method": "PUT", "path": "/channels/1/messages/2/reactions/👍/@me"});
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(!failed, "{text}");
    assert_eq!(mock.last().path, "/channels/1/messages/2/reactions/%F0%9F%91%8D/@me");
}

#[tokio::test]
async fn a_refusal_says_why_and_what_to_do() {
    let (_mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let (failed, text) = run(&tools, GET, json!({"path": "/guilds/9/roles", "jq": ".[]"})).await;
    assert!(failed);
    assert!(
        text.starts_with("403 Forbidden: Missing Permissions (code 50013)"),
        "{text}"
    );
    assert!(text.contains("role"), "{text}");
}

#[tokio::test]
async fn only_a_get_is_sent_again_after_a_server_error() {
    let (mock, base) = discord().await;
    let tools = tools(&base, PATIENT, options());
    let (failed, text) = run(&tools, GET, json!({"path": "/voice/regions"})).await;
    assert!(!failed, "{text}");
    assert_eq!(mock.hits("GET /voice/regions"), 2);
    let args = json!({"method": "POST", "path": "/channels/5/messages", "body": {"content": "once"}});
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(failed);
    assert!(text.starts_with("502 Bad Gateway"), "{text}");
    assert_eq!(mock.hits("POST /channels/5/messages"), 1);
}

#[tokio::test]
async fn a_path_outside_the_reference_needs_the_flag() {
    let (mock, base) = discord().await;
    let strict = tools(&base, PATIENT, options());
    let (failed, text) = run(&strict, GET, json!({"path": "/brand-new/thing"})).await;
    assert!(failed);
    assert!(text.contains("No Discord endpoint is GET /brand-new/thing"), "{text}");
    assert_eq!(mock.hits("GET /brand-new/thing"), 0);
    let open = tools(
        &base,
        PATIENT,
        Options {
            allow_unlisted: true,
            ..options()
        },
    );
    let (failed, text) = run(&open, GET, json!({"path": "/brand-new/thing"})).await;
    assert!(!failed, "{text}");
    assert_eq!(text, r#"{"new":true}"#);
}

#[tokio::test]
async fn read_only_refuses_changes() {
    let (mock, base) = discord().await;
    let tools = tools(
        &base,
        PATIENT,
        Options {
            read_only: true,
            ..options()
        },
    );
    let args = json!({"method": "POST", "path": "/channels/1/messages", "body": {"content": "hi"}});
    let (failed, text) = run(&tools, CALL, args).await;
    assert!(failed);
    assert!(text.contains("read-only"), "{text}");
    assert_eq!(mock.hits("POST /channels/1/messages"), 0);
}

#[tokio::test]
async fn a_long_answer_is_cut_and_says_so() {
    let (_mock, base) = discord().await;
    let tools = tools(
        &base,
        PATIENT,
        Options {
            max_output: 1_000,
            ..options()
        },
    );
    let (failed, text) = run(&tools, GET, json!({"path": "/guilds/1/members"})).await;
    assert!(!failed);
    assert!(
        text.contains("… cut: this is the first 1000 of"),
        "{}",
        &text[text.len() - 200..]
    );
    let (_, text) = run(&tools, GET, json!({"path": "/guilds/1/members", "jq": "length"})).await;
    assert_eq!(text, "500");
}

#[tokio::test]
async fn the_reference_is_fetched_cached_and_swapped_in() {
    let (mock, base) = discord().await;
    let cache = tempfile::tempdir().unwrap();
    let url = format!("{base}/spec.json");
    let first = update::refresh(cache.path(), &url).await.unwrap();
    assert!(
        matches!(first, update::Refresh::Updated { operations } if operations > 200),
        "{first:?}"
    );
    assert!(matches!(
        spec::current().unwrap().route("GET", "/fresh-endpoint"),
        spec::Route::Found(_)
    ));
    assert!(cache.path().join("openapi.json").is_file());
    // The bot token is for Discord alone.
    assert_eq!(mock.last().header("authorization"), "");
    // The next start asks again with the ETag, and an unchanged spec comes back as a bodiless 304.
    assert_eq!(
        update::refresh(cache.path(), &url).await.unwrap(),
        update::Refresh::Unchanged
    );
    assert_eq!(mock.hits("GET /spec.json"), 2);
    assert_eq!(mock.last().header("if-none-match"), "\"v1\"");
    // A later start reads the cache.
    assert!(update::load_cached(cache.path()));
    // Something that isn't the spec is turned down and nothing is cached.
    let other = tempfile::tempdir().unwrap();
    let error = update::refresh(other.path(), &format!("{base}/not-a-spec.json"))
        .await
        .unwrap_err();
    assert!(error.contains("isn't a usable spec"), "{error}");
    assert!(!other.path().join("openapi.json").exists());
}
