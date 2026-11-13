//! The binary as an MCP client meets it: JSON-RPC lines over stdin/stdout.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

async fn session(args: &[&str], requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_discord-mcp"))
        .args(args)
        .env_remove("DISCORD_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut answers = Vec::new();
    for request in requests {
        stdin.write_all(format!("{request}\n").as_bytes()).await.unwrap();
        // A notification has no answer to wait for.
        if request.get("id").is_none() {
            continue;
        }
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the server answers in time")
            .unwrap()
            .expect("the server is still there");
        answers.push(serde_json::from_str(&line).unwrap());
    }
    answers
}

fn hello() -> [Value; 2] {
    [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}
        }}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    ]
}

#[tokio::test]
async fn a_client_can_list_and_call_the_tools() {
    let mut requests = hello().to_vec();
    requests.push(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    requests.push(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "discord_docs", "arguments": {"query": "ban member"}
    }}));
    requests.push(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
        "name": "discord_get", "arguments": {"path": "/users/@me"}
    }}));
    let answers = session(&["--no-spec-update"], &requests).await;

    let info = &answers[0]["result"];
    assert_eq!(info["serverInfo"]["name"], "discord-mcp");
    assert!(info["instructions"].as_str().unwrap().contains("discord_docs"));

    let tools = answers[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["discord_docs", "discord_get", "discord_call"]);
    assert_eq!(tools[1]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[2]["annotations"]["destructiveHint"], true);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
    }
    let call = &tools[2]["inputSchema"];
    assert_eq!(call["required"], json!(["method", "path"]));

    let found = &answers[2]["result"];
    assert_eq!(found["isError"], false);
    assert!(
        found["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ban_user_from_guild")
    );

    let refused = &answers[3]["result"];
    assert_eq!(refused["isError"], true);
    assert!(
        refused["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("DISCORD_TOKEN")
    );
}

#[tokio::test]
async fn read_only_lists_no_write_tool() {
    let mut requests = hello().to_vec();
    requests.push(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let answers = session(&["--no-spec-update", "--read-only"], &requests).await;
    let tools = answers[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["discord_docs", "discord_get"]);
}
