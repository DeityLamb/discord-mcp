//! The API reference: Discord's spec, downloaded from its repository on the
//! first start and kept in the cache directory. Every later start uses the
//! cache at once and asks the repository whether it changed, sending the
//! cached copy's ETag: an unchanged spec costs a 304 with no body.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};

use crate::env_dir;
use crate::spec::{self, Spec};

pub const SPEC_URL: &str = "https://raw.githubusercontent.com/discord/discord-api-spec/main/specs/openapi.json";
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
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refresh {
    /// The repository has what the cache has.
    Unchanged,
    /// A newer spec is in use and in the cache.
    Updated { operations: usize },
}

/// `DISCORD_MCP_CACHE_DIR`, or the platform's cache directory for discord-mcp.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = env_dir("DISCORD_MCP_CACHE_DIR") {
        return Some(dir);
    }
    if cfg!(windows) {
        return env_dir("LOCALAPPDATA").map(|d| d.join("discord-mcp").join("cache"));
    }
    if cfg!(target_os = "macos") {
        return std::env::home_dir().map(|h| h.join("Library").join("Caches").join("discord-mcp"));
    }
    env_dir("XDG_CACHE_HOME")
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

/// Ask the repository for the spec, or whether the cached one changed: with the cached copy's ETag the
/// answer to "no" is a 304 and nothing more. Puts a new spec in use and in the cache.
pub async fn refresh(dir: &Path, url: &str) -> Result<Refresh, String> {
    let meta = read_meta(dir);
    let have_spec = dir.join(SPEC_FILE).is_file();
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
    if response.status() == StatusCode::NOT_MODIFIED {
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
    let meta = serde_json::to_vec(&Meta { etag }).unwrap_or_default();
    write_atomic(dir, META_FILE, &meta).map_err(|e| format!("writing {}: {e}", dir.display()))?;
    spec::replace(parsed);
    Ok(Refresh::Updated { operations })
}

/// Put the cached spec in use at once and, behind the scenes, download it if there is none or (unless
/// `update` is false) ask whether it changed. Failures are logged; with nothing cached, whoever waits
/// for the spec is told why there is none.
pub fn start(dir: PathBuf, url: String, update: bool) {
    let cached = load_cached(&dir);
    if cached && !update {
        return;
    }
    if !cached {
        // A cache that didn't load can't be what an ETag vouches for.
        let _ = std::fs::remove_file(dir.join(META_FILE));
    }
    tokio::spawn(async move {
        match refresh(&dir, &url).await {
            Ok(Refresh::Updated { operations }) => {
                tracing::info!("API reference downloaded from {url}: {operations} endpoints")
            }
            Ok(Refresh::Unchanged) => {}
            Err(why) => {
                tracing::warn!("API reference not downloaded: {why}");
                if !cached {
                    spec::unavailable(why);
                }
            }
        }
    });
}
