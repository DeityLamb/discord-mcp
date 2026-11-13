//! Discord's OpenAPI spec: the list of endpoints, matching a concrete path
//! to its endpoint, and finding endpoints by keyword. It is downloaded and
//! cached (`update`), not compiled in: until the first download finishes
//! there is none, and callers wait for it.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;

use crate::lock;

/// The spec in use: from the cache, or just downloaded. `None` until one is loaded.
static CURRENT: RwLock<Option<Arc<Spec>>> = RwLock::new(None);
/// Why there is no spec, once getting one has failed.
static MISSING: Mutex<Option<String>> = Mutex::new(None);
static CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

/// The spec in use, if one is loaded.
pub fn current() -> Option<Arc<Spec>> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Put a spec (from the cache or just fetched) in use.
pub fn replace(spec: Spec) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(spec));
    *lock(&MISSING) = None;
    CHANGED.notify_waiters();
}

/// Say that no spec is coming, and why: whoever waits for one hears it at once.
pub fn unavailable(why: String) {
    *lock(&MISSING) = Some(why);
    CHANGED.notify_waiters();
}

/// The spec in use, waiting up to `patience` for the first download. An error says why there is none.
pub async fn wait(patience: Duration) -> Result<Arc<Spec>, String> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let changed = CHANGED.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if let Some(spec) = current() {
            return Ok(spec);
        }
        if let Some(why) = lock(&MISSING).clone() {
            return Err(why);
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            return Err("it is still downloading".into());
        }
    }
}

/// The copy in the repository, for tests: what the cache holds after a download.
#[cfg(test)]
pub static FIXTURE: LazyLock<Spec> =
    LazyLock::new(|| Spec::parse(include_str!("../spec/openapi.json")).expect("the spec in the repository parses"));

pub const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// Path parameters that Discord keys rate-limit buckets on.
const MAJOR_PARAMS: [&str; 4] = ["channel_id", "guild_id", "webhook_id", "webhook_token"];

pub struct Spec {
    pub doc: Value,
    pub ops: Vec<Operation>,
}

pub struct Operation {
    pub method: &'static str,
    /// The path template, e.g. `/channels/{channel_id}/messages`.
    pub path: String,
    /// The spec's operationId, e.g. `create_message`.
    pub id: String,
    pub description: Option<String>,
    segments: Vec<Segment>,
    /// The id, path and description in lowercase, with every run of punctuation made one space:
    /// what keywords are looked for in.
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Param(String),
}

pub enum Route<'a> {
    Found(&'a Operation),
    /// The path is an endpoint, but not with this method.
    WrongMethod {
        path: &'a str,
        allowed: Vec<&'static str>,
    },
    /// No endpoint has this path; the closest ones.
    Unknown {
        nearest: Vec<&'a Operation>,
    },
}

impl Operation {
    pub fn title(&self) -> String {
        format!("{} {}", self.method, self.path)
    }

    /// The operation object in the spec.
    pub fn object<'a>(&self, spec: &'a Spec) -> &'a Value {
        &spec.doc["paths"][&self.path][self.method.to_ascii_lowercase()]
    }

    /// Path-level and operation-level parameters, in that order.
    pub fn parameters<'a>(&self, spec: &'a Spec) -> Vec<&'a Value> {
        let shared = spec.doc["paths"][&self.path]["parameters"].as_array();
        let own = self.object(spec)["parameters"].as_array();
        shared.into_iter().chain(own).flatten().collect()
    }

    /// The key requests to `concrete` (a path this operation matched) are
    /// rate-limited under: the template with its major parameters filled in.
    pub fn bucket_key(&self, concrete: &str) -> String {
        let mut key = String::from(self.method);
        for (segment, value) in self.segments.iter().zip(concrete.split('/').skip(1)) {
            key.push('/');
            match segment {
                Segment::Literal(s) => key.push_str(s),
                Segment::Param(name) if MAJOR_PARAMS.contains(&name.as_str()) => key.push_str(value),
                Segment::Param(name) => {
                    key.push('{');
                    key.push_str(name);
                    key.push('}');
                }
            }
        }
        key
    }

    fn literals(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| matches!(s, Segment::Literal(_)))
            .count()
    }

    fn matches(&self, concrete: &[&str]) -> bool {
        self.segments.len() == concrete.len() && self.segments.iter().zip(concrete).all(|(s, c)| s.matches(c))
    }
}

impl Segment {
    /// Does a part of a concrete path fit here? A parameter takes anything.
    fn matches(&self, concrete: &str) -> bool {
        match self {
            Self::Literal(literal) => literal == concrete,
            Self::Param(_) => true,
        }
    }
}

impl Spec {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let doc: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        let paths = doc["paths"].as_object().ok_or("the spec has no paths")?;
        let mut ops = Vec::new();
        for (path, item) in paths {
            for method in METHODS {
                let Some(op) = item.get(method.to_ascii_lowercase()) else {
                    continue;
                };
                let id = op["operationId"].as_str().unwrap_or_default().to_owned();
                let description = op["description"].as_str().map(str::to_owned);
                let segments: Vec<Segment> = path
                    .split('/')
                    .skip(1)
                    .map(|s| match s.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                        Some(name) => Segment::Param(name.to_owned()),
                        None => Segment::Literal(s.to_owned()),
                    })
                    .collect();
                let text = plain(&format!("{id} {path} {}", description.as_deref().unwrap_or("")));
                ops.push(Operation {
                    method,
                    path: path.clone(),
                    id,
                    description,
                    segments,
                    text,
                });
            }
        }
        Ok(Self { doc, ops })
    }

    /// The endpoint a request to `path` (concrete, as [`normalize_path`] returns it) reaches.
    pub fn route(&self, method: &str, path: &str) -> Route<'_> {
        let concrete: Vec<&str> = path.split('/').skip(1).collect();
        // `/users/@me` is both `/users/@me` and `/users/{user_id}`: the template with more literal parts wins.
        let best = self
            .ops
            .iter()
            .filter(|op| op.matches(&concrete))
            .max_by_key(|op| op.literals());
        let Some(best) = best else {
            return Route::Unknown {
                nearest: self.nearest(&concrete),
            };
        };
        let on_path = self.on_template(&best.path);
        match on_path.iter().find(|op| op.method == method) {
            Some(op) => Route::Found(op),
            None => Route::WrongMethod {
                path: &best.path,
                allowed: on_path.iter().map(|op| op.method).collect(),
            },
        }
    }

    /// Endpoints whose path starts like `concrete`, longest shared start first.
    fn nearest(&self, concrete: &[&str]) -> Vec<&Operation> {
        let shared = |op: &Operation| {
            op.segments
                .iter()
                .zip(concrete)
                .take_while(|(s, c)| s.matches(c))
                .count()
        };
        // Where the paths part ways, a misspelt word (`memberz`) still starts like the right one.
        let alike = |op: &Operation, shared: usize| match (op.segments.get(shared), concrete.get(shared)) {
            (Some(Segment::Literal(l)), Some(c)) => l.chars().zip(c.chars()).take_while(|(a, b)| a == b).count(),
            _ => 0,
        };
        let mut scored: Vec<(usize, usize, usize, &Operation)> = self
            .ops
            .iter()
            .map(|op| {
                let shared = shared(op);
                (
                    shared,
                    alike(op, shared),
                    op.segments.len().abs_diff(concrete.len()),
                    op,
                )
            })
            .filter(|(shared, ..)| *shared > 0)
            .collect();
        scored.sort_by_key(|&(shared, alike, length_diff, _)| (Reverse(shared), Reverse(alike), length_diff));
        scored.into_iter().take(8).map(|(.., op)| op).collect()
    }

    /// An operation by its operationId, any case.
    pub fn by_id(&self, id: &str) -> Option<&Operation> {
        self.ops.iter().find(|op| op.id.eq_ignore_ascii_case(id))
    }

    /// Every operation on a path template.
    pub fn on_template(&self, template: &str) -> Vec<&Operation> {
        self.ops.iter().filter(|op| op.path == template).collect()
    }

    /// Endpoints whose name, path or description contains any of the keywords, with how many each
    /// contains: most first, then by path. No keywords: every endpoint.
    pub fn find(&self, keywords: &[String]) -> Vec<(&Operation, usize)> {
        let keywords: Vec<String> = keywords.iter().map(|k| plain(k)).filter(|k| !k.is_empty()).collect();
        let mut found: Vec<(&Operation, usize)> = self
            .ops
            .iter()
            .map(|op| (op, keywords.iter().filter(|k| op.text.contains(k.as_str())).count()))
            .filter(|(_, n)| *n > 0 || keywords.is_empty())
            .collect();
        found.sort_by_key(|&(op, count)| (Reverse(count), op.path.as_str(), op.method));
        found
    }

    /// Names of types containing any of the keywords, shortest first.
    pub fn find_schemas(&self, keywords: &[String], limit: usize) -> Vec<&str> {
        let keywords: Vec<String> = keywords
            .iter()
            .map(|k| plain(k).replace(' ', ""))
            .filter(|k| !k.is_empty())
            .collect();
        let Some(schemas) = self.doc["components"]["schemas"].as_object() else {
            return Vec::new();
        };
        let mut names: Vec<&str> = schemas
            .keys()
            .filter(|name| {
                let lower = name.to_ascii_lowercase();
                keywords.iter().any(|k| lower.contains(k.as_str()))
            })
            .map(String::as_str)
            .collect();
        names.sort_by_key(|n| n.len());
        names.truncate(limit);
        names
    }

    /// A schema by name, any case.
    pub fn schema(&self, name: &str) -> Option<(&str, &Value)> {
        let schemas = self.doc["components"]["schemas"].as_object()?;
        schemas
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(n, v)| (n.as_str(), v))
    }

    /// What a `$ref` points at.
    pub fn resolve(&self, reference: &str) -> Option<&Value> {
        self.doc.pointer(reference.strip_prefix('#')?)
    }
}

/// Lowercase, with every run of characters other than letters, digits and `@` made one space:
/// `create_thread`, `create-thread` and `Create Thread` all read `create thread`.
fn plain(text: &str) -> String {
    text.split(|c: char| !(c.is_alphanumeric() || c == '@'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A path as an agent wrote it, made into what goes after the API base:
/// `/channels/123/messages`, plus any query string it carried.
pub fn normalize_path(input: &str) -> Result<(String, Vec<(String, String)>), String> {
    let mut rest = input.trim();
    for prefix in ["https://discord.com", "https://discordapp.com"] {
        if let Some(r) = rest.strip_prefix(prefix) {
            rest = r;
        }
    }
    if rest.contains("://") {
        return Err(format!(
            "path must be a Discord API path like /channels/123/messages, not a URL to another site: {input}"
        ));
    }
    if let Some(r) = rest.strip_prefix("/api") {
        rest = r;
        // /v10, /v9, …
        if let Some(after) = rest.strip_prefix("/v") {
            let digits = after.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 && (after.len() == digits || after[digits..].starts_with('/')) {
                rest = &after[digits..];
            }
        }
    }
    if rest.contains('#') {
        return Err(format!("path can't contain '#': {input}"));
    }
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, parse_query(query)),
        None => (rest, Vec::new()),
    };
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Err("path is empty: give an API path like /users/@me".into());
    }
    let mut out = String::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("path has an empty or relative part: {input}"));
        }
        let placeholder = segment
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .or_else(|| segment.strip_prefix('<').and_then(|s| s.strip_suffix('>')))
            .or_else(|| {
                segment
                    .strip_prefix(':')
                    .filter(|s| s.ends_with("_id") || s.ends_with(".id"))
            });
        if let Some(name) = placeholder {
            return Err(format!(
                "path still has the placeholder {segment}: put the real {name} there"
            ));
        }
        out.push('/');
        out.push_str(&encode_segment(segment));
    }
    Ok((out, query))
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(byte) = escape_at(bytes, i) {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The byte a `%XX` escape starting at `i` stands for, if one does.
fn escape_at(bytes: &[u8], i: usize) -> Option<u8> {
    let [b'%', high, low] = *bytes.get(i..i + 3)? else {
        return None;
    };
    let digit = |b: u8| char::from(b).to_digit(16);
    Some((digit(high)? << 4 | digit(low)?) as u8)
}

/// Percent-encode what a path segment can't hold (an emoji in a reaction
/// path, a space), leaving escapes that are already there alone.
fn encode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) || escape_at(bytes, i).is_some();
        push_encoded(&mut out, b, keep);
    }
    out
}

/// Percent-encode a header value (the audit log reason takes URL-encoded UTF-8).
pub fn encode_header(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        push_encoded(&mut out, b, b.is_ascii_alphanumeric() || b"-._~ ".contains(&b));
    }
    out
}

/// One byte of percent-encoded text: as it is when `keep` (always ASCII then), else `%XX`.
fn push_encoded(out: &mut String, b: u8, keep: bool) {
    if keep {
        out.push(char::from(b));
    } else {
        let _ = write!(out, "%{b:02X}");
    }
}

/// Distinct path templates, for telling an agent what exists under a prefix.
pub fn templates<'a>(ops: impl IntoIterator<Item = &'a Operation>) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    ops.into_iter()
        .map(|op| op.path.as_str())
        .filter(|p| seen.insert(*p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(method: &str, path: &str) -> &'static Operation {
        match FIXTURE.route(method, path) {
            Route::Found(op) => op,
            Route::WrongMethod { path, allowed } => panic!("wrong method on {path}: {allowed:?}"),
            Route::Unknown { .. } => panic!("unknown route {method} {path}"),
        }
    }

    #[test]
    fn spec_has_the_endpoints() {
        assert!(FIXTURE.ops.len() >= 200, "{} operations", FIXTURE.ops.len());
        assert!(FIXTURE.by_id("create_message").is_some());
    }

    #[test]
    fn routes_prefer_literal_parts() {
        assert_eq!(found("GET", "/users/@me").id, "get_my_user");
        assert_eq!(found("GET", "/users/123").id, "get_user");
        assert_eq!(found("GET", "/channels/1/messages/pins").id, "list_pins");
        assert_eq!(found("GET", "/channels/1/messages/2").id, "get_message");
        assert_eq!(found("POST", "/channels/1/messages").id, "create_message");
        assert_eq!(
            found("PATCH", "/webhooks/1/tok/messages/@original").id,
            "update_original_webhook_message"
        );
    }

    #[test]
    fn wrong_method_names_the_allowed_ones() {
        match FIXTURE.route("POST", "/users/@me") {
            Route::WrongMethod { path, allowed } => {
                assert_eq!(path, "/users/@me");
                assert_eq!(allowed, ["GET", "PATCH"]);
            }
            _ => panic!("expected a wrong method"),
        }
    }

    #[test]
    fn unknown_route_suggests_neighbours() {
        match FIXTURE.route("GET", "/guilds/1/memberz") {
            Route::Unknown { nearest } => {
                assert_eq!(nearest[0].path, "/guilds/{guild_id}/members");
                assert!(nearest.iter().all(|op| op.path.starts_with("/guilds/")));
            }
            _ => panic!("expected an unknown route"),
        }
    }

    #[test]
    fn bucket_key_keeps_major_parameters_only() {
        let op = found("GET", "/channels/55/messages/77");
        assert_eq!(
            op.bucket_key("/channels/55/messages/77"),
            "GET/channels/55/messages/{message_id}"
        );
    }

    #[test]
    fn keywords_match_names_paths_and_descriptions_as_written() {
        let ids = |keywords: &[&str]| {
            let keywords: Vec<String> = keywords.iter().map(|k| k.to_string()).collect();
            FIXTURE
                .find(&keywords)
                .iter()
                .map(|(op, _)| op.id.clone())
                .collect::<Vec<_>>()
        };
        let threads = ids(&["thread"]);
        assert!(threads.contains(&"create_thread".to_owned()));
        assert!(threads.contains(&"create_thread_from_message".to_owned()));
        // Any keyword is enough; one that matches more comes first.
        let kick = ids(&["kick", "remove member", "guild member"]);
        assert!(kick.contains(&"delete_guild_member".to_owned()));
        assert!(ids(&["audit log"]).contains(&"list_guild_audit_log_entries".to_owned()));
        assert!(ids(&["Bulk-Delete"]).contains(&"bulk_delete_messages".to_owned()));
        let both = ids(&["ban", "guild"]);
        assert!(both.iter().position(|id| id == "ban_user_from_guild") < both.iter().position(|id| id == "get_guild"));
        assert!(ids(&["zzzz"]).is_empty());
        assert_eq!(ids(&[]).len(), FIXTURE.ops.len());
    }

    #[test]
    fn paths_are_normalized() {
        let path = |s: &str| normalize_path(s).map(|(p, _)| p);
        assert_eq!(path("/users/@me").unwrap(), "/users/@me");
        assert_eq!(path("users/@me/").unwrap(), "/users/@me");
        assert_eq!(path("https://discord.com/api/v10/guilds/1").unwrap(), "/guilds/1");
        assert_eq!(path("/api/guilds/1").unwrap(), "/guilds/1");
        assert_eq!(
            path("/channels/1/messages/2/reactions/👍/@me").unwrap(),
            "/channels/1/messages/2/reactions/%F0%9F%91%8D/@me"
        );
        assert_eq!(
            path("/channels/1/messages/2/reactions/%F0%9F%91%8D/@me").unwrap(),
            "/channels/1/messages/2/reactions/%F0%9F%91%8D/@me"
        );
        assert!(path("/channels/{channel_id}/messages").is_err());
        assert!(path("https://example.com/x").is_err());
        assert!(path("/guilds/../users").is_err());
        let (p, q) = normalize_path("/channels/1/messages?limit=5&around=9").unwrap();
        assert_eq!(p, "/channels/1/messages");
        assert_eq!(q, [("limit".into(), "5".into()), ("around".into(), "9".into())]);
    }

    #[test]
    fn header_values_are_encoded() {
        assert_eq!(encode_header("spam, résumé"), "spam%2C r%C3%A9sum%C3%A9");
    }
}
