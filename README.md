# discord-mcp

[![CI](https://github.com/DeityLamb/discord-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/DeityLamb/discord-mcp/actions/workflows/ci.yml)

**The whole Discord API for your agent, in five tools.** Every one of the 250 REST endpoints, every
gateway event, and the API reference to find them, acting as your bot.

| Tool | Covers |
| --- | --- |
| `discord_docs` | The API reference, built from [Discord's OpenAPI spec](https://github.com/discord/discord-api-spec) and refreshed daily: search endpoints, read one in full, look up a type. |
| `discord_get` | Every `GET` endpoint: servers, channels, messages, members, roles, threads, emojis... |
| `discord_call` | Every `POST`, `PUT`, `PATCH` and `DELETE`: send, edit, react, moderate, manage, with file uploads and audit-log reasons. |
| `discord_events` | Every gateway event, live: messages, reactions, joins, edits. Wait for one, filter, page by cursor. |
| `discord_gateway` | Gateway commands: the bot's status and activity, member lookups with online status. |

There is no tool per endpoint, so nothing is missing and new endpoints work the day Discord adds them.
The agent looks the endpoint up in `discord_docs` and calls it, and a `jq` argument on every call keeps
long responses short. Rate limits are handled for it.

## Install

```sh
# Linux, macOS
curl -fsSL https://raw.githubusercontent.com/DeityLamb/discord-mcp/main/install.sh | sh

# Windows (PowerShell)
irm https://raw.githubusercontent.com/DeityLamb/discord-mcp/main/install.ps1 | iex

# With Cargo: a prebuilt binary, or built from source
cargo binstall discord-mcp
cargo install discord-mcp
```

Or download a binary from [Releases](https://github.com/DeityLamb/discord-mcp/releases).

## Set up

1. Create an application at <https://discord.com/developers/applications>, add a bot, copy its token,
   and invite it to your server. To read message text, switch on **Message Content Intent** on the
   Bot page; to list members, **Server Members Intent**.
2. Add the server to your client. For Claude Code:

   ```sh
   claude mcp add discord -s user -e DISCORD_TOKEN=<bot token> -- discord-mcp
   ```

   Elsewhere: command `discord-mcp`, with `DISCORD_TOKEN` in its environment.

Without a token, `discord_docs` still works.

## Options

| Flag | Effect |
| --- | --- |
| `--read-only` | Nothing on Discord can be changed: no `discord_call` or `discord_gateway`. |
| `--disable-gateway` | REST only: no events, no background process, the bot stays offline. |
| `--intents <list>` | Gateway intents, e.g. `guilds,guild_messages,message_content`. Default: all but typing and presences, plus the privileged ones switched on for the bot. |
| `--no-hub` | Keep the gateway in this process instead of sharing it. |
| `--allow-unlisted` | Allow paths the API reference doesn't list. |
| `--no-spec-update` | Use the built-in API reference only. |
| `--max-output <chars>` | Longest answer of one call (default 40000). |

`discord-mcp call <tool> '<json>'` runs one call from a shell.

## Several clients at once

A bot should have one gateway connection, but each client starts its own server. So the first one
starts a background hub, which keeps the connection, the event buffer and the rate limits, and every
session talks to it over loopback, authenticated by a secret file only you can read. Each session
reads events by its own cursor, without taking them from the others. The hub leaves a minute after
its last session.

## Safety

- The agent can do whatever the bot can. Give the bot only the permissions you want used, or run
  `--read-only`.
- Discord content is written by other people and may contain instructions aimed at the agent; keep
  `discord_call` behind your client's approval prompt.
- `discord_call` uploads local files it is given paths to.
- The token goes to Discord only: it is never printed, and `jq` can't read the environment.

Not supported: voice, and sharding (bots in 2500+ servers).

## Development

```sh
cargo test
```

The tests run the real binary against a stand-in Discord REST API and gateway; no token needed.

## License

GPL-3.0-or-later. The bundled `spec/openapi.json` is Discord's, under the MIT license in `spec/LICENSE`.
