//! The hub: one background process per bot that every MCP session on this
//! machine shares. It holds what must exist once: the gateway session, the
//! event log and the rate-limit state. A session's own process is only a
//! pipe to it ([`crate::pipe`]).
//!
//! The hub listens on loopback and speaks MCP on each connection, after a
//! one-line handshake carrying the secret from the hub file, which only the
//! owning user can read.

use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use crate::gateway::Gateway;
use crate::mcp::DiscordServer;
use crate::rest::Rest;
use crate::tools::{Options, Tools};

/// How long a hub with no sessions stays up, so a restarted client finds its events still there.
pub const IDLE_GRACE: Duration = Duration::from_secs(60);
/// Time a new connection gets to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a starting hub waits for one that is leaving to let go of the lock.
const LOCK_PATIENCE: Duration = Duration::from_secs(10);
/// A hello is a secret and a few options; anything longer isn't one.
pub const MAX_HELLO: usize = 64 * 1024;

/// Where a hub listens and what a session must present; written when it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubFile {
    pub port: u16,
    pub secret: String,
    pub pid: u32,
    pub version: String,
}

/// A session's first line to the hub.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub secret: String,
    /// What this session may do and where its files are.
    pub options: Options,
}

/// The hub's answer; MCP starts after it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Welcome {
    pub ok: bool,
    pub error: Option<String>,
}

/// What every session of a hub shares.
pub struct Shared {
    pub rest: Arc<Rest>,
    pub gateway: Option<Arc<Gateway>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Ran until it had been without sessions for the grace period.
    Served,
    /// Another hub for the same bot holds the lock; this one has nothing to do.
    AlreadyRunning,
}

/// `DISCORD_MCP_STATE_DIR`, or the per-user directory hubs keep their files in.
pub fn state_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("DISCORD_MCP_STATE_DIR") {
        return Some(dir);
    }
    if cfg!(windows) {
        return var("LOCALAPPDATA").map(|d| d.join("discord-mcp"));
    }
    if cfg!(target_os = "macos") {
        return std::env::home_dir().map(|h| h.join("Library").join("Application Support").join("discord-mcp"));
    }
    var("XDG_RUNTIME_DIR")
        .or_else(|| var("XDG_STATE_HOME"))
        .or_else(|| std::env::home_dir().map(|h| h.join(".local").join("state")))
        .map(|d| d.join("discord-mcp"))
}

/// Which hub a session belongs to: sessions agreeing on all of `parts` (the token, the API base, the
/// intents, the build) share one. The token goes in hashed; the key is safe in a file name.
pub fn key(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.len().to_le_bytes());
        hash.update(part.as_bytes());
    }
    hex(&hash.finalize()[..8])
}

pub fn file_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("hub-{key}.json"))
}

pub fn log_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("hub-{key}.log"))
}

fn lock_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("hub-{key}.lock"))
}

/// `None` when no hub has written the file.
pub fn read_file(path: &Path) -> io::Result<Option<HubFile>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Temp file + rename, so a session never reads half a file.
fn write_file(path: &Path, file: &HubFile) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent directory"))?;
    let tmp = dir.join(format!(".hub.{}.tmp", std::process::id()));
    write_private(&tmp, &serde_json::to_vec(file).map_err(io::Error::other)?)?;
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    // A user's local application data is theirs alone already.
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

fn secret() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compare without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// One line, read a byte at a time so nothing after it is taken from the stream: what follows is MCP's.
pub async fn read_line<R: AsyncRead + Unpin>(reader: &mut R, max: usize) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        line.push(byte);
        if line.len() > max {
            return Err(io::Error::other("line too long"));
        }
    }
    String::from_utf8(line).map_err(io::Error::other)
}

/// `value` as one line of JSON: the handshake's other half to [`read_line`].
pub async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

/// Be the hub for `key` until no session has been attached for `idle_grace`.
pub async fn run(dir: &Path, key: &str, shared: Shared, idle_grace: Duration) -> Result<Outcome, String> {
    create_private_dir(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    // The lock is held for as long as this process lives, and the system lets go of it if the process dies.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path(dir, key))
        .map_err(|e| format!("can't open the hub lock: {e}"))?;
    let file = file_path(dir, key);
    let patient_until = Instant::now() + LOCK_PATIENCE;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            // Someone holds it: a hub that is serving, or one that is on its way out and has already
            // taken its file away. Only the first is a reason to step aside.
            Err(std::fs::TryLockError::WouldBlock) => {
                if serving(&file).await || Instant::now() >= patient_until {
                    return Ok(Outcome::AlreadyRunning);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(format!("can't take the hub lock: {e}")),
        }
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| format!("can't listen on loopback: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let secret = Arc::new(secret().map_err(|e| format!("no random bytes for the hub secret: {e}"))?);
    let written = HubFile {
        port,
        secret: secret.to_string(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    write_file(&file, &written).map_err(|e| format!("can't write {}: {e}", file.display()))?;
    tracing::info!("hub {key} listening on 127.0.0.1:{port}");

    let shared = Arc::new(shared);
    let sessions = Arc::new(AtomicUsize::new(0));
    let ended = Arc::new(Notify::new());
    let mut idle_since = Some(Instant::now());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        tracing::warn!("accept failed: {e}");
                        continue;
                    }
                };
                sessions.fetch_add(1, Ordering::SeqCst);
                idle_since = None;
                let (shared, secret, sessions, ended) = (shared.clone(), secret.clone(), sessions.clone(), ended.clone());
                tokio::spawn(async move {
                    if let Err(why) = session(stream, &secret, &shared).await {
                        tracing::debug!("session ended: {why}");
                    }
                    sessions.fetch_sub(1, Ordering::SeqCst);
                    ended.notify_one();
                });
            }
            () = ended.notified() => {
                if sessions.load(Ordering::SeqCst) == 0 {
                    idle_since = Some(Instant::now());
                }
            }
            () = until(idle_since.map(|since| since + idle_grace)) => break,
        }
    }
    tracing::info!("hub {key}: no sessions for {} s, leaving", idle_grace.as_secs());
    // Make room first, then say goodbye: a session arriving now starts a new hub and needn't wait
    // for this one to finish with Discord.
    drop(listener);
    if read_file(&file).ok().flatten().as_ref() == Some(&written) {
        let _ = std::fs::remove_file(&file);
    }
    drop(lock);
    if let Some(gateway) = &shared.gateway {
        gateway.stop().await;
    }
    Ok(Outcome::Served)
}

/// Is there a hub listening where the file says?
async fn serving(file: &Path) -> bool {
    let Ok(Some(hub)) = read_file(file) else {
        return false;
    };
    let connect = TcpStream::connect((Ipv4Addr::LOCALHOST, hub.port));
    matches!(
        tokio::time::timeout(Duration::from_millis(500), connect).await,
        Ok(Ok(_))
    )
}

/// One session: the handshake, then MCP until the other side leaves.
async fn session(mut stream: TcpStream, secret: &str, shared: &Shared) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let line = tokio::time::timeout(HELLO_TIMEOUT, read_line(&mut stream, MAX_HELLO))
        .await
        .map_err(|_| "no hello in time".to_owned())?
        .map_err(|e| format!("reading the hello: {e}"))?;
    let hello = match serde_json::from_str::<Hello>(&line) {
        Err(_) => Err("that isn't a discord-mcp hello"),
        Ok(hello) if !same(hello.secret.as_bytes(), secret.as_bytes()) => Err("wrong secret"),
        Ok(hello) => Ok(hello),
    };
    let welcome = Welcome {
        ok: hello.is_ok(),
        error: hello.as_ref().err().map(|&refusal| refusal.to_owned()),
    };
    write_line(&mut stream, &welcome)
        .await
        .map_err(|e| format!("answering the hello: {e}"))?;
    let hello = hello?;
    let server = DiscordServer {
        tools: Tools {
            rest: Some(shared.rest.clone()),
            gateway: shared.gateway.clone(),
            options: hello.options,
        },
    };
    let service = server
        .serve(stream)
        .await
        .map_err(|e| format!("MCP handshake failed: {e}"))?;
    service
        .waiting()
        .await
        .map(drop)
        .map_err(|e| format!("MCP service crashed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_tell_bots_and_builds_apart() {
        let a = key(&["token-a", "https://discord.com/api/v10", "auto"]);
        assert_eq!(a.len(), 16);
        assert_eq!(a, key(&["token-a", "https://discord.com/api/v10", "auto"]));
        assert_ne!(a, key(&["token-b", "https://discord.com/api/v10", "auto"]));
        // Parts don't run into each other.
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
        assert!(!a.contains("token"));
    }

    #[test]
    fn the_hub_file_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_path(dir.path(), "k");
        assert_eq!(read_file(&path).unwrap(), None);
        let file = HubFile {
            port: 4711,
            secret: "s".into(),
            pid: 1,
            version: "0".into(),
        };
        write_file(&path, &file).unwrap();
        assert_eq!(read_file(&path).unwrap(), Some(file));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn secrets_compare_in_full() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert_eq!(secret().unwrap().len(), 64);
        assert_ne!(secret().unwrap(), secret().unwrap());
    }

    #[tokio::test]
    async fn a_line_is_read_without_taking_what_follows() {
        let mut input: &[u8] = b"{\"hello\":1}\nrest";
        assert_eq!(read_line(&mut input, 100).await.unwrap(), "{\"hello\":1}");
        assert_eq!(input, b"rest");
        let mut long: &[u8] = b"aaaaaaaaaaaaaaaaaaaaaaaaa\n";
        assert!(read_line(&mut long, 10).await.is_err());
    }
}
