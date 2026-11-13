//! Discord's OpenAPI spec, embedded: the list of endpoints, matching a
//! concrete path to its endpoint, and keyword search over them.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, RwLock};

use serde_json::Value;

const RAW: &str = include_str!("../spec/openapi.json");

/// The copy of the spec compiled into the binary: what a first or offline run has.
pub static EMBEDDED: LazyLock<Arc<Spec>> =
    LazyLock::new(|| Arc::new(Spec::parse(RAW).expect("the embedded OpenAPI spec parses")));

static CURRENT: LazyLock<RwLock<Arc<Spec>>> = LazyLock::new(|| RwLock::new(EMBEDDED.clone()));

/// The spec in use: the embedded copy until a newer one is loaded.
pub fn current() -> Arc<Spec> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Put a newer spec (from the cache or just fetched) in use.
pub fn replace(spec: Spec) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(spec);
}

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
    /// Lowercase words of the id, the path's literal parts and the description.
    words: Vec<String>,
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

pub struct Search<'a> {
    /// Best first; only the endpoints that match the most words.
    pub hits: Vec<&'a Operation>,
    /// How many of the query's words those endpoints match.
    pub matched: usize,
    /// How many words the query has, small words aside.
    pub words: usize,
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
        self.segments.len() == concrete.len()
            && self.segments.iter().zip(concrete).all(|(s, c)| match s {
                Segment::Literal(l) => l == c,
                Segment::Param(_) => true,
            })
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
                let mut words = words_of(&id);
                for segment in &segments {
                    if let Segment::Literal(s) = segment {
                        words.extend(words_of(s));
                    }
                }
                if let Some(d) = &description {
                    words.extend(words_of(d));
                }
                ops.push(Operation {
                    method,
                    path: path.clone(),
                    id,
                    description,
                    segments,
                    words,
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
        let on_path: Vec<&Operation> = self.ops.iter().filter(|op| op.path == best.path).collect();
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
                .take_while(|(s, c)| match s {
                    Segment::Literal(l) => l == *c,
                    Segment::Param(_) => true,
                })
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
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
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

    /// Endpoints matching the query's words.
    pub fn search(&self, query: &str, limit: usize) -> Search<'_> {
        let tokens: Vec<String> = words_of(query)
            .into_iter()
            .filter(|w| !STOP_WORDS.contains(&w.as_str()))
            .collect();
        let mut hits: Vec<(usize, u32, &Operation)> = Vec::new();
        for op in &self.ops {
            let mut matched = 0;
            let mut score = 0;
            for token in &tokens {
                let s = token_score(token, op);
                if s > 0 {
                    matched += 1;
                    score += s;
                }
            }
            if matched > 0 {
                hits.push((matched, score, op));
            }
        }
        let most = hits.iter().map(|h| h.0).max().unwrap_or(0);
        hits.retain(|h| h.0 == most);
        // On equal scores the endpoint named in fewer words is the plainer one: create_message before
        // create_lobby_message, list_pins before deprecated_list_pins.
        let name_words = |op: &Operation| op.id.split('_').count();
        hits.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(name_words(a.2).cmp(&name_words(b.2)))
                .then(a.2.path.len().cmp(&b.2.path.len()))
                .then(a.2.method.cmp(b.2.method))
        });
        Search {
            hits: hits.into_iter().take(limit).map(|(_, _, op)| op).collect(),
            matched: most,
            words: tokens.len(),
        }
    }

    /// Names of schemas containing every word of the query.
    pub fn search_schemas(&self, query: &str, limit: usize) -> Vec<&str> {
        let tokens = words_of(query);
        let Some(schemas) = self.doc["components"]["schemas"].as_object() else {
            return Vec::new();
        };
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut names: Vec<&str> = schemas
            .keys()
            .filter(|name| {
                let lower = name.to_ascii_lowercase();
                tokens.iter().all(|t| lower.contains(singular(t)))
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

const STOP_WORDS: [&str; 18] = [
    "a", "an", "the", "to", "in", "of", "for", "on", "from", "and", "with", "how", "do", "i", "by", "is", "at", "into",
];

/// What people call a verb or a thing, and what the spec calls it.
const SYNONYMS: [(&str, &[&str]); 24] = [
    ("send", &["create", "execute"]),
    ("post", &["create"]),
    ("write", &["create"]),
    ("make", &["create"]),
    ("new", &["create"]),
    ("edit", &["update"]),
    ("modify", &["update"]),
    ("change", &["update"]),
    ("rename", &["update"]),
    ("patch", &["update"]),
    ("set", &["update", "set"]),
    ("remove", &["delete", "remove"]),
    ("clear", &["delete"]),
    ("revoke", &["delete", "revoke"]),
    ("fetch", &["get", "list"]),
    ("read", &["get", "list"]),
    ("show", &["get", "list"]),
    ("find", &["search", "get", "list"]),
    ("get", &["get", "list"]),
    ("list", &["list", "get"]),
    ("server", &["guild"]),
    ("user", &["user", "member"]),
    ("react", &["reaction"]),
    ("slash", &["command"]),
];

/// Words that mean one endpoint the spec names differently.
const ALIASES: [(&str, &[&str]); 16] = [
    ("kick", &["delete_guild_member"]),
    ("timeout", &["update_guild_member"]),
    ("mute", &["update_guild_member"]),
    ("deafen", &["update_guild_member"]),
    ("nickname", &["update_guild_member", "update_my_guild_member"]),
    ("nick", &["update_guild_member", "update_my_guild_member"]),
    ("purge", &["bulk_delete_messages"]),
    ("history", &["list_messages"]),
    ("reply", &["create_message"]),
    ("upload", &["create_message"]),
    ("attachment", &["create_message"]),
    ("file", &["create_message"]),
    ("embed", &["create_message"]),
    ("unpin", &["delete_pin"]),
    ("topic", &["update_channel"]),
    ("slowmode", &["update_channel"]),
];

/// How well one query word matches an endpoint: 0 is no match.
fn token_score(token: &str, op: &Operation) -> u32 {
    if ALIASES
        .iter()
        .any(|(word, ids)| *word == singular(token) && ids.contains(&op.id.as_str()))
    {
        return 5;
    }
    let alternatives = SYNONYMS
        .iter()
        .find(|(word, _)| *word == token)
        .map(|(_, alts)| alts.to_vec());
    let direct = word_score(token, op);
    let via_synonym = alternatives
        .into_iter()
        .flatten()
        .map(|alt| word_score(alt, op))
        .max()
        .unwrap_or(0);
    direct.max(via_synonym)
}

fn word_score(word: &str, op: &Operation) -> u32 {
    // The id's words come first in `words`; a hit there says more than one in the description.
    let id_words = op.id.split('_').count();
    let mut best = 0;
    for (i, have) in op.words.iter().enumerate() {
        let weight = if i < id_words { 2 } else { 1 };
        let hit = if have == word || singular(have) == singular(word) {
            2
        } else if word.len() >= 3 && have.starts_with(word) {
            1
        } else {
            0
        };
        best = best.max(hit * weight);
    }
    best
}

fn singular(word: &str) -> &str {
    match word.strip_suffix('s') {
        Some(stem) if stem.len() >= 3 && !stem.ends_with('s') => stem,
        _ => word,
    }
}

/// Lowercase alphanumeric runs (`@me` stays whole).
fn words_of(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '@'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
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
        match bytes[i] {
            b'%' if i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]) => {
                out.push(hex_value(bytes[i + 1]) << 4 | hex_value(bytes[i + 2]));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_value(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    }
}

/// Percent-encode what a path segment can't hold (an emoji in a reaction
/// path, a space), leaving escapes that are already there alone.
fn encode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let escaped = b == b'%' && i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]);
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) || escaped {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        i += 1;
    }
    out
}

/// Percent-encode a header value (the audit log reason takes URL-encoded UTF-8).
pub fn encode_header(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~ ".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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
        match EMBEDDED.route(method, path) {
            Route::Found(op) => op,
            Route::WrongMethod { path, allowed } => panic!("wrong method on {path}: {allowed:?}"),
            Route::Unknown { .. } => panic!("unknown route {method} {path}"),
        }
    }

    #[test]
    fn spec_has_the_endpoints() {
        assert!(EMBEDDED.ops.len() >= 200, "{} operations", EMBEDDED.ops.len());
        assert!(EMBEDDED.by_id("create_message").is_some());
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
        match EMBEDDED.route("POST", "/users/@me") {
            Route::WrongMethod { path, allowed } => {
                assert_eq!(path, "/users/@me");
                assert_eq!(allowed, ["GET", "PATCH"]);
            }
            _ => panic!("expected a wrong method"),
        }
    }

    #[test]
    fn unknown_route_suggests_neighbours() {
        match EMBEDDED.route("GET", "/guilds/1/memberz") {
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
    fn search_understands_everyday_words() {
        let top = |q: &str| {
            EMBEDDED
                .search(q, 5)
                .hits
                .first()
                .map(|op| op.id.clone())
                .unwrap_or_default()
        };
        assert_eq!(top("send message"), "create_message");
        assert_eq!(top("kick member"), "delete_guild_member");
        assert_eq!(top("ban user"), "ban_user_from_guild");
        assert_eq!(top("edit message"), "update_message");
        assert_eq!(top("list server channels"), "list_guild_channels");
        assert_eq!(top("purge messages"), "bulk_delete_messages");
        assert_eq!(top("create thread"), "create_thread");
        assert!(EMBEDDED.search("zzzz qqqq", 5).hits.is_empty());
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
