//! discord-mcp: MCP server for Discord.
//!
//!   discord-mcp [options]                       MCP over stdio
//!   discord-mcp call <tool> [json-args | @file | -]   run one tool call and print the result
//!   discord-mcp call --list                     print each tool's name, description and arguments
//!
//! The bot token comes from DISCORD_TOKEN. With a token, sessions on one
//! machine share a background hub (started by the first, gone a minute after
//! the last), so there is one gateway connection however many clients run.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use discord_mcp::gateway::{self, Gateway, IntentChoice};
use discord_mcp::rest::{self, Rest};
use discord_mcp::tools::{Options, Tools};
use discord_mcp::{hub, mcp, pipe, update};
use rmcp::ServiceExt;
use rmcp::model::JsonObject;
use serde_json::Value;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Leave out discord_call: nothing on Discord can be changed.
    #[arg(long, global = true)]
    read_only: bool,
    /// Don't connect to Discord's gateway: no discord_events, no background hub, and the bot stays offline.
    #[arg(long, global = true)]
    disable_gateway: bool,
    /// Gateway intents to ask for, by name: guilds,guild_messages,message_content. Default: all but typing
    /// and presences, with the privileged ones the bot has switched on.
    #[arg(long, global = true, value_name = "LIST")]
    intents: Option<String>,
    /// Keep the gateway in this process instead of sharing a background hub with other sessions.
    #[arg(long, global = true)]
    no_hub: bool,
    /// Also send requests to paths the API reference doesn't list.
    #[arg(long, global = true)]
    allow_unlisted: bool,
    /// Keep the cached API reference: download it once, then never check for a newer one.
    #[arg(long, global = true)]
    no_spec_update: bool,
    /// Most characters one tool call returns.
    #[arg(long, global = true, default_value_t = 40_000, value_name = "CHARS")]
    max_output: usize,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP on stdin/stdout. This is what runs without a subcommand.
    Mcp,
    /// Run one tool call through the same code the MCP server uses and print the result.
    Call {
        /// Print every tool's name, description and arguments instead of calling one.
        #[arg(long)]
        list: bool,
        /// A tool name, e.g. discord_get. Required unless --list.
        tool: Option<String>,
        /// The tool's arguments as a JSON object, e.g. '{"path": "/users/@me"}'; @file reads them from a
        /// file and - from stdin. Default: {}.
        args: Option<String>,
    },
    /// Be the hub that sessions share. Sessions start it themselves.
    #[command(hide = true)]
    Hub {
        /// Which hub this is: what its sessions agree on.
        #[arg(long)]
        key: String,
    },
}

/// Without a state directory there is nowhere for a hub's files.
const NO_STATE_DIR: &str = "no directory to keep the hub's files in; set DISCORD_MCP_STATE_DIR";

/// What comes from the environment and decides which bot this is and how it connects.
struct Setup {
    token: Option<String>,
    base: String,
    intents: IntentChoice,
    /// A stand-in gateway, in tests.
    gateway_url: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DISCORD_MCP_LOG")
                .unwrap_or_else(|_| "warn,discord_mcp=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let outcome = match &cli.command {
        None | Some(Command::Mcp) => serve(&cli).await,
        Some(Command::Call { list, tool, args }) => call(&cli, *list, tool.as_deref(), args.clone()).await,
        Some(Command::Hub { key }) => be_hub(&cli, key).await,
    };
    let code = outcome.unwrap_or_else(|e| {
        eprintln!("discord-mcp: {e}");
        ExitCode::FAILURE
    });
    // Leave at once: a read of stdin that is still waiting would keep the runtime from shutting down.
    std::process::exit(if code == ExitCode::SUCCESS { 0 } else { 1 });
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn setup(cli: &Cli) -> Result<Setup, String> {
    let intents = match &cli.intents {
        Some(list) => IntentChoice::Fixed(gateway::parse_intents(list).map_err(|e| format!("--intents: {e}"))?),
        None => IntentChoice::Auto,
    };
    Ok(Setup {
        token: env("DISCORD_TOKEN"),
        base: env("DISCORD_API_BASE").unwrap_or_else(|| rest::DEFAULT_BASE.into()),
        intents,
        gateway_url: env("DISCORD_GATEWAY_URL"),
    })
}

fn options(cli: &Cli) -> Options {
    Options {
        read_only: cli.read_only,
        allow_unlisted: cli.allow_unlisted,
        max_output: cli.max_output.max(1_000),
        cwd: std::env::current_dir().unwrap_or_default(),
    }
}

/// The tools with everything in this process: REST if there is a token, the gateway if it's wanted too.
fn tools(cli: &Cli, setup: &Setup) -> Result<Tools, String> {
    let rest = match &setup.token {
        Some(token) => Some(Arc::new(
            Rest::new(token, &setup.base, rest::DEFAULT_PATIENCE).map_err(|e| format!("DISCORD_TOKEN: {e}"))?,
        )),
        None => {
            tracing::warn!("DISCORD_TOKEN isn't set: only discord_docs will work");
            None
        }
    };
    let gateway = match (&setup.token, &rest) {
        (Some(token), Some(rest)) if !cli.disable_gateway => Some(Gateway::new(
            token,
            rest.clone(),
            setup.intents,
            setup.gateway_url.clone(),
        )),
        _ => None,
    };
    Ok(Tools {
        rest,
        gateway,
        options: options(cli),
    })
}

/// Put the cached API reference in use and fetch it behind the scenes: the first time, or (unless
/// `--no-spec-update`) to see whether it changed.
fn reference(cli: &Cli) {
    let dir = update::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("discord-mcp"));
    let url = env("DISCORD_MCP_SPEC_URL").unwrap_or_else(|| update::SPEC_URL.into());
    update::start(dir, url, !cli.no_spec_update);
}

/// Which hub the sessions of this bot share. A rebuilt or upgraded binary gets a hub of its own
/// rather than joining one that runs older code.
fn hub_key(setup: &Setup, token: &str) -> String {
    let build = std::env::current_exe()
        .and_then(std::fs::metadata)
        .map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            format!("{modified}-{}", m.len())
        })
        .unwrap_or_default();
    let intents = match setup.intents {
        IntentChoice::Auto => "auto".to_owned(),
        IntentChoice::Fixed(bits) => bits.to_string(),
    };
    hub::key(&[
        token,
        &setup.base,
        &intents,
        setup.gateway_url.as_deref().unwrap_or(""),
        env!("CARGO_PKG_VERSION"),
        &build,
    ])
}

async fn serve(cli: &Cli) -> Result<ExitCode, String> {
    let setup = setup(cli)?;
    if let Some(token) = &setup.token
        && !cli.disable_gateway
        && !cli.no_hub
    {
        match through_hub(cli, &setup, token).await {
            Ok(pipe::End::ClientLeft) => return Ok(ExitCode::SUCCESS),
            Ok(pipe::End::HubLeft) => return Err("the hub closed the connection; reconnect to start a new one".into()),
            Err(why) => tracing::warn!("serving from this process, without a hub: {why}"),
        }
    }
    let server = mcp::DiscordServer {
        tools: tools(cli, &setup)?,
    };
    let gateway = server.tools.gateway.clone();
    reference(cli);
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("MCP handshake failed: {e}"))?;
    let outcome = service
        .waiting()
        .await
        .map(drop)
        .map_err(|e| format!("MCP service crashed: {e}"));
    if let Some(gateway) = gateway {
        gateway.stop().await;
    }
    outcome.map(|()| ExitCode::SUCCESS)
}

/// Be a pipe between the MCP client and the hub, starting the hub if this is the first session.
async fn through_hub(cli: &Cli, setup: &Setup, token: &str) -> Result<pipe::End, String> {
    let dir = hub::state_dir().ok_or(NO_STATE_DIR)?;
    let key = hub_key(setup, token);
    let mut hub_args = Vec::new();
    if cli.no_spec_update {
        hub_args.push("--no-spec-update".to_owned());
    }
    if let Some(intents) = &cli.intents {
        hub_args.extend(["--intents".to_owned(), intents.clone()]);
    }
    hub_args.extend(["hub".to_owned(), "--key".to_owned(), key.clone()]);
    let stream = pipe::attach(&dir, &key, &options(cli), &hub_args).await?;
    Ok(pipe::carry(stream).await)
}

async fn be_hub(cli: &Cli, key: &str) -> Result<ExitCode, String> {
    let setup = setup(cli)?;
    let tools = tools(cli, &setup)?;
    let rest = tools.rest.ok_or("DISCORD_TOKEN isn't set")?;
    let dir = hub::state_dir().ok_or(NO_STATE_DIR)?;
    let grace = env("DISCORD_MCP_HUB_GRACE")
        .and_then(|s| s.parse().ok())
        .map_or(hub::IDLE_GRACE, Duration::from_secs_f64);
    reference(cli);
    let shared = hub::Shared {
        rest,
        gateway: tools.gateway,
    };
    match hub::run(&dir, key, shared, grace).await? {
        hub::Outcome::Served => {}
        hub::Outcome::AlreadyRunning => tracing::info!("hub {key} is already running"),
    }
    Ok(ExitCode::SUCCESS)
}

async fn call(cli: &Cli, list: bool, tool: Option<&str>, args: Option<String>) -> Result<ExitCode, String> {
    let setup = setup(cli)?;
    let tools = tools(cli, &setup)?;
    if list {
        // Written at once: a reader that stops early (`| head`) ends this quietly.
        let _ = std::io::stdout().write_all(tool_list(&tools).as_bytes());
        return Ok(ExitCode::SUCCESS);
    }
    let Some(tool) = tool else {
        return Err("needs a tool name, e.g. discord_docs (or --list to see them all)".into());
    };
    let args = call_args(args)?;
    reference(cli);
    let result = tools.call(tool, args).await;
    if let Some(gateway) = &tools.gateway {
        gateway.stop().await;
    }
    for block in &result.content {
        if let Some(text) = block.as_text() {
            println!("{}", text.text);
        }
    }
    Ok(if result.is_error.unwrap_or(false) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `call --list`: each tool's name and description, then its arguments, one per line.
fn tool_list(tools: &Tools) -> String {
    let mut text = String::new();
    for t in tools.definitions() {
        text += &format!("{}\n  {}\n", t.name, t.description.as_deref().unwrap_or(""));
        let required: Vec<&str> = t
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(properties) = t.input_schema.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, schema) in properties {
            let doc = schema.get("description").and_then(Value::as_str).unwrap_or("");
            let must = if required.contains(&name.as_str()) {
                " (required)"
            } else {
                ""
            };
            text += &format!(
                "    {name}{must}: {}\n",
                doc.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
    }
    text
}

/// A call's json-args: given inline, read from a file (`@file`) or from stdin (`-`). `null` is none.
fn call_args(args: Option<String>) -> Result<Option<JsonObject>, String> {
    let text = match args {
        None => return Ok(None),
        Some(dash) if dash == "-" => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| format!("reading json-args from stdin: {e}"))?;
            text
        }
        Some(text) => match text.strip_prefix('@') {
            Some(file) => std::fs::read_to_string(file).map_err(|e| format!("reading json-args from {file}: {e}"))?,
            None => text,
        },
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => Ok(Some(object)),
        Ok(Value::Null) => Ok(None),
        Ok(_) => Err("json-args must be a JSON object, e.g. '{\"path\": \"/users/@me\"}'".into()),
        Err(e) => Err(format!("json-args is not valid JSON: {e}")),
    }
}
