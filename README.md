# discord-mcp

An MCP server that gives an agent the whole Discord API through four tools, acting as your bot.

| Tool | What it does |
| --- | --- |
| `discord_docs` | The Discord REST API reference: search endpoints, read one in full, look up a type. |
| `discord_get` | Any `GET` endpoint. Read-only, so a client can allow it without asking. |
| `discord_call` | Any `POST`, `PUT`, `PATCH` or `DELETE` endpoint, with file uploads and an audit-log reason. |
| `discord_events` | What happens as it happens (messages, reactions, members joining), from the gateway. |

There is no tool per endpoint. The agent finds the endpoint in `discord_docs`, then calls it; a `jq`
argument on every call keeps long responses out of its context.

## Setup

1. Create an application at <https://discord.com/developers/applications>, add a bot, and copy the
   bot token from its **Bot** page. Invite the bot to your server with the permissions you want it to have.
2. To read what other people write, switch on **Message Content Intent** on the same page. To list
   members, switch on **Server Members Intent**.
3. Build and install:

   ```sh
   cargo install --path .
   ```

4. Add it to your MCP client with the token in its environment:

   ```json
   {
     "mcpServers": {
       "discord": {
         "command": "discord-mcp",
         "env": { "DISCORD_TOKEN": "your bot token" }
       }
     }
   }
   ```

Without a token the server still starts, and `discord_docs` works.

## Options

| Flag | Effect |
| --- | --- |
| `--read-only` | Leaves out `discord_call`: nothing on Discord can be changed. |
| `--disable-gateway` | No `discord_events`, no background hub, and the bot stays offline. |
| `--intents <list>` | Gateway intents by name, e.g. `guilds,guild_messages,message_content`. By default: all but typing and presences, plus the privileged ones the bot has switched on. |
| `--no-hub` | Keeps the gateway in the session's own process instead of sharing a hub. |
| `--allow-unlisted` | Also sends requests to paths the API reference doesn't list. |
| `--no-spec-update` | Uses the API reference compiled into the binary and never fetches a newer one. |
| `--max-output <chars>` | Most characters one tool call returns (default 40000). |

`discord-mcp call <tool> '<json>'` runs one tool call from a shell, and `discord-mcp call --list`
prints the tools with their arguments.

## Several clients at once

Each MCP client starts its own `discord-mcp`, but a bot should have one gateway connection, not one
per client. So the first session starts a background **hub** and every session is a thin pipe to it:

```
client 1 ── stdio ── discord-mcp ─┐
client 2 ── stdio ── discord-mcp ─┼─ loopback ── hub ── REST + gateway ── Discord
client 3 ── stdio ── discord-mcp ─┘
```

- The hub holds the gateway session, the event buffer and the rate-limit state.
- It listens on loopback only; a session must present a secret from a file only your user can read.
- It leaves a minute after its last session, saying goodbye to Discord so the bot goes offline.
- The gateway connects on the first `discord_events` call, so sessions that only use REST never open it.
- Sessions keep their own options: one can be `--read-only` while another is not.

## Waiting for something

Events are buffered, and each has a cursor. A reader never takes events from another reader. To act
and then wait for the response:

1. `discord_events {}` returns the current cursor.
2. `discord_call` makes the change, for example sends a message.
3. `discord_events {"after": <cursor>, "types": ["MESSAGE_CREATE"], "filter": ".channel_id == \"…\"", "wait": 60}`
   returns as soon as a matching event arrives.

## What to know before giving it to an agent

- **It acts as your bot.** Whatever the bot may do on Discord, the agent may do. Give the bot only the
  permissions you want used, and use `--read-only` where reading is enough.
- **Discord content is untrusted.** Messages, names and topics are written by other people and may
  contain instructions aimed at the agent. The tool descriptions say so; keep `discord_call` behind
  your client's approval prompt.
- **Uploads read local files.** `discord_call` sends the files it is given paths to.
- **The token stays put.** It is sent to Discord and nowhere else, never printed, and `jq` filters
  cannot read the environment.

## Files it keeps

- The API reference, refreshed at most once a day from
  [discord/discord-api-spec](https://github.com/discord/discord-api-spec): the user cache directory
  (`~/.cache/discord-mcp` on Linux), or `DISCORD_MCP_CACHE_DIR`.
- The hub's address file, lock and log: `$XDG_RUNTIME_DIR/discord-mcp` on Linux, or
  `DISCORD_MCP_STATE_DIR`.

`DISCORD_MCP_LOG` sets the log filter (for example `debug`); logs go to stderr, and the hub's to its log file.

## Not supported

Voice, sharding (bots in 2500 or more servers), and sending gateway commands such as presence updates.

## Development

```sh
cargo test
```

The tests run the real binary against a stand-in for Discord's REST API and gateway; no token or
network is needed. To refresh the bundled reference:

```sh
curl -sSL -o spec/openapi.json https://raw.githubusercontent.com/discord/discord-api-spec/main/specs/openapi.json
```

## License

GPL-3.0-or-later; see `LICENSE`. The bundled `spec/openapi.json` is Discord's, under the MIT license in
`spec/LICENSE`.
