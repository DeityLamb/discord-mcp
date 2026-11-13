//! One request to Discord's REST API as the bot: authorization, uploads,
//! waiting out rate limits, and reading whatever comes back.

use std::error::Error as _;
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use reqwest::multipart::{Form, Part};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::filter;
use crate::ratelimit::{Observed, RateLimiter};
use crate::spec::encode_header;

pub const DEFAULT_BASE: &str = "https://discord.com/api/v10";
/// Longest one call waits on rate limits before it gives up and says when to retry.
pub const DEFAULT_PATIENCE: Duration = Duration::from_secs(30);
/// Sends of one call, counting the retries after a 429 or a failed GET.
const MAX_ATTEMPTS: u32 = 5;
/// Waited on top of what Discord says, so a request doesn't land a moment early.
const SLACK: Duration = Duration::from_millis(50);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Rest {
    http: reqwest::Client,
    base: String,
    authorization: String,
    limits: RateLimiter,
    patience: Duration,
}

pub struct Request {
    pub method: Method,
    /// What goes after the API base: `/channels/123/messages`.
    pub path: String,
    /// The key its rate limit is tracked under.
    pub bucket: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    pub files: Vec<FilePart>,
    /// How the body travels next to files.
    pub layout: Layout,
    /// Written to the server's audit log.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The body as one `payload_json` field: messages and most uploads.
    PayloadJson,
    /// Each top-level key of the body as its own form field: stickers.
    Fields,
}

pub struct FilePart {
    /// The multipart field: `files[0]`, `file`.
    pub field: String,
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

pub struct Response {
    pub status: StatusCode,
    pub body: Body,
    /// Time spent waiting on rate limits before the answer.
    pub waited: Duration,
}

pub enum Body {
    Empty,
    Json(Value),
    Text(String),
    Binary { content_type: String, bytes: usize },
}

#[derive(Debug)]
pub enum RestError {
    /// Rate limited for longer than the call may wait.
    RateLimited { retry_after: Duration, global: bool },
    /// The request never got an answer.
    Transport(String),
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited { retry_after, global } => write!(
                f,
                "Discord is rate limiting {}: try again in {:.0} s.",
                if *global { "the bot as a whole" } else { "this endpoint" },
                retry_after.as_secs_f64().ceil()
            ),
            Self::Transport(why) => write!(f, "The request didn't reach Discord: {why}"),
        }
    }
}

impl Rest {
    /// `token` is the bot token, with or without the `Bot ` in front.
    pub fn new(token: &str, base: &str, patience: Duration) -> Result<Self, String> {
        let token = bare_token(token);
        if token.is_empty() || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("the bot token is empty or has spaces in it".into());
        }
        let http = reqwest::Client::builder()
            .user_agent(user_agent())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("can't set up the HTTP client: {e}"))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            authorization: format!("Bot {token}"),
            limits: RateLimiter::default(),
            patience,
        })
    }

    pub async fn send(&self, request: &Request) -> Result<Response, RestError> {
        let deadline = Instant::now() + self.patience;
        let mut waited = Duration::ZERO;
        let mut attempts = 0;
        loop {
            while let Err(wait) = self.limits.claim(&request.bucket, Instant::now()) {
                let wait = wait + SLACK;
                if Instant::now() + wait > deadline {
                    return Err(RestError::RateLimited {
                        retry_after: wait,
                        global: false,
                    });
                }
                tokio::time::sleep(wait).await;
                waited += wait;
            }
            attempts += 1;
            // Only a GET is safe to send twice: a POST that got no answer may still have happened.
            let retryable = request.method == Method::GET && attempts < 3;
            let response = match self.build(request)?.send().await {
                Ok(response) => response,
                Err(e) if retryable && (e.is_connect() || e.is_timeout()) => {
                    back_off(attempts).await;
                    continue;
                }
                Err(e) => return Err(RestError::Transport(describe(&e))),
            };
            let status = response.status();
            let headers = response.headers();
            self.limits.observe(&request.bucket, &observed(headers), Instant::now());
            if status == StatusCode::TOO_MANY_REQUESTS {
                // Taken before the body, which consumes the response.
                let header_seconds = header_number(headers, RETRY_AFTER.as_str());
                let header_global = header_text(headers, "x-ratelimit-global") == Some("true")
                    || header_text(headers, "x-ratelimit-scope") == Some("global");
                // Discord's own 429 has a JSON body; one from the network edge in front of it has only the header.
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let seconds = body["retry_after"].as_f64().or(header_seconds).unwrap_or(1.0);
                let global = body["global"].as_bool().unwrap_or(false) || header_global;
                let retry_after = Duration::from_secs_f64(seconds.clamp(0.0, 3600.0));
                self.limits
                    .limited(&request.bucket, retry_after, global, Instant::now());
                if attempts >= MAX_ATTEMPTS || Instant::now() + retry_after + SLACK > deadline {
                    return Err(RestError::RateLimited { retry_after, global });
                }
                continue;
            }
            if status.is_server_error() && retryable {
                back_off(attempts).await;
                continue;
            }
            let body = read(response).await?;
            return Ok(Response { status, body, waited });
        }
    }

    fn build(&self, request: &Request) -> Result<reqwest::RequestBuilder, RestError> {
        let url = format!("{}{}", self.base, request.path);
        let mut builder = self
            .http
            .request(request.method.clone(), url)
            .header(AUTHORIZATION, &self.authorization)
            .timeout(if request.files.is_empty() {
                TIMEOUT
            } else {
                UPLOAD_TIMEOUT
            });
        if !request.query.is_empty() {
            builder = builder.query(&request.query);
        }
        if let Some(reason) = &request.reason {
            builder = builder.header("X-Audit-Log-Reason", encode_header(reason));
        }
        if !request.files.is_empty() {
            let mut form = Form::new();
            match (&request.body, request.layout) {
                (Some(body), Layout::PayloadJson) => {
                    let part = Part::text(body.to_string())
                        .mime_str("application/json")
                        .map_err(|e| RestError::Transport(e.to_string()))?;
                    form = form.part("payload_json", part);
                }
                (Some(Value::Object(fields)), Layout::Fields) => {
                    for (name, value) in fields {
                        form = form.text(name.clone(), filter::plain(value));
                    }
                }
                _ => {}
            }
            for file in &request.files {
                let part = Part::bytes(file.data.clone())
                    .file_name(file.name.clone())
                    .mime_str(&file.mime)
                    .map_err(|e| RestError::Transport(format!("{}: {e}", file.name)))?;
                form = form.part(file.field.clone(), part);
            }
            builder = builder.multipart(form);
        } else if let Some(body) = &request.body {
            builder = builder.json(body);
        } else if request.method != Method::GET && request.method != Method::DELETE {
            // Discord answers 411 to a bodiless POST without a length.
            builder = builder.header(reqwest::header::CONTENT_LENGTH, "0");
        }
        Ok(builder)
    }
}

/// The token without the `Bot ` an agent or a copied header may have put in front, and without spaces
/// around it.
pub fn bare_token(token: &str) -> &str {
    let token = token.trim();
    token.strip_prefix("Bot ").unwrap_or(token).trim()
}

fn user_agent() -> String {
    // Discord asks for `DiscordBot ($url, $version)`.
    let url = match env!("CARGO_PKG_REPOSITORY") {
        "" => env!("CARGO_PKG_NAME"),
        url => url,
    };
    format!("DiscordBot ({url}, {})", env!("CARGO_PKG_VERSION"))
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    header_text(headers, name).and_then(|v| v.trim().parse().ok())
}

fn observed(headers: &HeaderMap) -> Observed {
    Observed {
        bucket: header_text(headers, "x-ratelimit-bucket").map(str::to_owned),
        remaining: header_text(headers, "x-ratelimit-remaining").and_then(|v| v.trim().parse().ok()),
        reset_after: header_number(headers, "x-ratelimit-reset-after"),
    }
}

/// The pause before retrying a GET that failed: longer after each attempt.
async fn back_off(attempts: u32) {
    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempts))).await;
}

async fn read(response: reqwest::Response) -> Result<Body, RestError> {
    let content_type = header_text(response.headers(), CONTENT_TYPE.as_str())
        .unwrap_or_default()
        .to_owned();
    let bytes = response.bytes().await.map_err(|e| RestError::Transport(describe(&e)))?;
    if bytes.is_empty() {
        return Ok(Body::Empty);
    }
    if content_type.contains("json")
        && let Ok(value) = serde_json::from_slice(&bytes)
    {
        return Ok(Body::Json(value));
    }
    if content_type.contains("json") || content_type.starts_with("text/") || content_type.is_empty() {
        return Ok(Body::Text(String::from_utf8_lossy(&bytes).into_owned()));
    }
    Ok(Body::Binary {
        content_type,
        bytes: bytes.len(),
    })
}

/// A reqwest error with its causes, which is where the useful part is.
fn describe(error: &reqwest::Error) -> String {
    let mut text = if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "couldn't connect".to_owned()
    } else {
        "request failed".to_owned()
    };
    let mut cause = error.source();
    while let Some(inner) = cause {
        text.push_str(&format!(": {inner}"));
        cause = inner.source();
    }
    text
}
