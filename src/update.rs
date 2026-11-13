//! Keeping the API reference current: the spec is fetched from Discord's
//! repository at most once a day and kept in the cache directory. The copy
//! compiled into the binary is what a first or offline run uses.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};

use crate::spec::{self, Spec};

pub const SPEC_URL: &str = "https://raw.githubusercontent.com/discord/discord-api-spec/main/specs/openapi.json";
/// How long a fetched spec is used before asking whether there's a newer one.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(30);
/// The spec is about 1.2 MB; anything far past that isn't it.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// A document with fewer endpoints than this isn't Discord's API.
const MIN_OPERATIONS: usize = 100;

const SPEC_FILE: &str = "openapi.json";
const META_FILE: &str = "openapi.meta.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    /// The `ETag` the cached spec came with.
    etag: Option<String>,
    /// When the repository was last asked, in seconds since the epoch.
    checked: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refresh {
    /// Asked within the last day; nothing was sent.
    Fresh,
    /// The repository has what the cache has.
    Unchanged,
    /// A newer spec is in use and in the cache.
    Updated { operations: usize },
}

/// `DISCORD_MCP_CACHE_DIR`, or the platform's cache directory for discord-mcp.
pub fn cache_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("DISCORD_MCP_CACHE_DIR") {
        return Some(dir);
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA").map(|d| d.join("discord-mcp").join("cache"));
    }
    if cfg!(target_os = "macos") {
        return std::env::home_dir().map(|h| h.join("Library").join("Caches").join("discord-mcp"));
    }
    var("XDG_CACHE_HOME")
        .or_else(|| std::env::home_dir().map(|h| h.join(".cache")))
        .map(|d| d.join("discord-mcp"))
}

/// Put the cached spec in use, if there is a good one. Says whether it did.
pub fn load_cached(dir: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(dir.join(SPEC_FILE)) else {
        return false;
    };
    match checked(&raw) {
        Ok(parsed) => {
            spec::replace(parsed);
            true
        }
        Err(why) => {
            tracing::warn!("ignoring the cached API reference: {why}");
            false
        }
    }
}

fn checked(raw: &str) -> Result<Spec, String> {
    let parsed = Spec::parse(raw)?;
    if parsed.ops.len() < MIN_OPERATIONS {
        return Err(format!("only {} endpoints in it", parsed.ops.len()));
    }
    Ok(parsed)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_meta(dir: &Path) -> Meta {
    std::fs::read(dir.join(META_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Temp file + rename, so a reader never sees half a file.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dir.join(name))
}

/// Ask the repository for a newer spec if the last time was over a day ago.
pub async fn refresh(dir: &Path, url: &str) -> Result<Refresh, String> {
    let meta = read_meta(dir);
    let have_spec = dir.join(SPEC_FILE).is_file();
    if have_spec && now().saturating_sub(meta.checked) < MAX_AGE.as_secs() {
        return Ok(Refresh::Fresh);
    }
    // Its own client: the bot token never goes anywhere but Discord.
    let http = reqwest::Client::builder()
        .user_agent(concat!("discord-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = http.get(url);
    if let Some(etag) = meta.etag.as_deref().filter(|_| have_spec) {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = request.send().await.map_err(|e| format!("fetching {url}: {e}"))?;
    let checked_now = |etag: Option<String>| {
        let meta = Meta { etag, checked: now() };
        let bytes = serde_json::to_vec(&meta).unwrap_or_default();
        write_atomic(dir, META_FILE, &bytes).map_err(|e| format!("writing {}: {e}", dir.display()))
    };
    if response.status() == StatusCode::NOT_MODIFIED {
        checked_now(meta.etag)?;
        return Ok(Refresh::Unchanged);
    }
    if !response.status().is_success() {
        return Err(format!("fetching {url}: {}", response.status()));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.map_err(|e| format!("reading {url}: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{url} sent {} bytes, far more than the spec is", bytes.len()));
    }
    let raw = std::str::from_utf8(&bytes).map_err(|_| format!("{url} isn't text"))?;
    let parsed = checked(raw).map_err(|why| format!("{url} isn't a usable spec: {why}"))?;
    let operations = parsed.ops.len();
    write_atomic(dir, SPEC_FILE, &bytes).map_err(|e| format!("writing {}: {e}", dir.display()))?;
    checked_now(etag)?;
    spec::replace(parsed);
    Ok(Refresh::Updated { operations })
}

/// Use the cache now and refresh it behind the scenes. Failures are logged and otherwise ignored: the
/// reference in use stays the one already loaded.
pub fn start(dir: PathBuf, url: String) {
    load_cached(&dir);
    tokio::spawn(async move {
        match refresh(&dir, &url).await {
            Ok(Refresh::Updated { operations }) => {
                tracing::info!("API reference updated from {url}: {operations} endpoints")
            }
            Ok(_) => {}
            Err(why) => tracing::warn!("API reference not updated: {why}"),
        }
    });
}
