//! A session's own process when there is a hub: it finds the hub (starting
//! one if there is none), says hello, and from then on only carries bytes
//! between the MCP client on stdin/stdout and the hub.

use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::hub::{self, Hello, Welcome};
use crate::tools::Options;

/// Longest a session waits for a hub it started to answer.
const HUB_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const WELCOME_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_EVERY: Duration = Duration::from_millis(100);
/// How long a started hub gets to answer before another is started.
const RESTART_EVERY: Duration = Duration::from_secs(4);

#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// The MCP client closed stdin: the session is over.
    ClientLeft,
    /// The hub closed the connection.
    HubLeft,
}

/// Attach to the hub for `key`, starting it with `hub_args` if none answers.
pub async fn attach(dir: &Path, key: &str, options: &Options, hub_args: &[String]) -> Result<TcpStream, String> {
    let deadline = Instant::now() + HUB_TIMEOUT;
    let mut started: Option<Instant> = None;
    loop {
        let why = match greet(dir, key, options).await {
            Ok(stream) => return Ok(stream),
            Err(why) => why,
        };
        // Whatever went wrong (no hub yet, one that just left, a stale file), a hub of our own settles it:
        // if another is alive it holds the lock and ours steps aside. One that stepped aside for a hub
        // that then left is tried again.
        if started.is_none_or(|at| at.elapsed() >= RESTART_EVERY) {
            start_hub(dir, key, hub_args)?;
            started = Some(Instant::now());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no hub answered within {} s ({why}); its log is {}",
                HUB_TIMEOUT.as_secs(),
                hub::log_path(dir, key).display()
            ));
        }
        tokio::time::sleep(RETRY_EVERY).await;
    }
}

/// Connect to the hub the file names and get its welcome.
async fn greet(dir: &Path, key: &str, options: &Options) -> Result<TcpStream, String> {
    let file = hub::read_file(&hub::file_path(dir, key))
        .map_err(|e| format!("unreadable hub file: {e}"))?
        .ok_or("no hub is running")?;
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((Ipv4Addr::LOCALHOST, file.port)))
        .await
        .map_err(|_| "the hub didn't accept in time".to_owned())?
        .map_err(|e| format!("the hub isn't listening: {e}"))?;
    let _ = stream.set_nodelay(true);
    let hello = Hello {
        secret: file.secret,
        options: options.clone(),
    };
    hub::write_line(&mut stream, &hello)
        .await
        .map_err(|e| format!("saying hello: {e}"))?;
    let answer = tokio::time::timeout(WELCOME_TIMEOUT, hub::read_line(&mut stream, hub::MAX_HELLO))
        .await
        .map_err(|_| "the hub didn't answer the hello".to_owned())?
        .map_err(|e| format!("reading the hub's answer: {e}"))?;
    let welcome: Welcome = serde_json::from_str(&answer).map_err(|_| "that port isn't a hub".to_owned())?;
    if !welcome.ok {
        return Err(format!("the hub refused: {}", welcome.error.unwrap_or_default()));
    }
    Ok(stream)
}

/// Start a hub that outlives this process. Its stderr goes to a log next to the hub file.
fn start_hub(dir: &Path, key: &str, hub_args: &[String]) -> Result<(), String> {
    hub::create_private_dir(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("can't find this program to start the hub: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(hub::log_path(dir, key))
        .map_err(|e| format!("can't open the hub log: {e}"))?;
    let mut command = std::process::Command::new(exe);
    command
        .args(hub_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    detach(&mut command);
    let mut child = command.spawn().map_err(|e| format!("can't start the hub: {e}"))?;
    // A hub that finds another already running exits at once; collect it so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Its own process group: closing the terminal or stopping the client doesn't take the hub with it.
#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut std::process::Command) {}

/// Carry bytes both ways until one side is done.
pub async fn carry(stream: TcpStream) -> End {
    let (mut from_hub, mut to_hub) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        let _ = tokio::io::copy(&mut stdin, &mut to_hub).await;
        let _ = to_hub.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut from_hub, &mut stdout).await;
        let _ = stdout.flush().await;
    };
    tokio::select! {
        () = up => End::ClientLeft,
        () = down => End::HubLeft,
    }
}
