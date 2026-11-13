# Simple MCP for Discord

[![CI](https://github.com/DeityLamb/discord-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/DeityLamb/discord-mcp/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/discord-mcp.svg)](https://crates.io/crates/discord-mcp)

`discord-mcp` gives your agent Discord's REST API, gateway events and API reference, and acts as your bot.

<https://discord-mcp.deitylamb.dev>

## Install

One binary, nothing else to set up.

```sh
# cargo
cargo binstall discord-mcp

# linux, macos
curl -fsSL https://raw.githubusercontent.com/DeityLamb/discord-mcp/main/install.sh | sh

# windows
irm https://raw.githubusercontent.com/DeityLamb/discord-mcp/main/install.ps1 | iex
```

Or download it from [Releases](https://github.com/DeityLamb/discord-mcp/releases), or build it with
`cargo install discord-mcp`.

## Set up

Create a bot in the Developer Portal and copy its token.

<https://discord.com/developers/applications>

Invite it with this link (the ID is on General Information).

```
https://discord.com/oauth2/authorize?client_id=<id>&scope=bot&permissions=0
```

Add it to your agent with the token.

```sh
# claude code
claude mcp add discord -s user -e DISCORD_TOKEN=<token> -- discord-mcp

# codex
codex mcp add discord --env DISCORD_TOKEN=<token> -- discord-mcp

# pi
pi mcp add discord --env DISCORD_TOKEN=<token> -- discord-mcp
```

```jsonc
// claude_desktop_config.json, or any other client's MCP config
{
  "mcpServers": {
    "discord": {
      "command": "discord-mcp",
      "env": { "DISCORD_TOKEN": "<token>" }
    }
  }
}
```

To read what people write, switch on **Message Content Intent** on the bot's page in the portal; to list
members, **Server Members Intent**.

## Tools

`discord_get` and `discord_call` take any API path, so there is no tool per endpoint. Every call can take a
`jq` filter to shorten the answer.

| Tool | |
| --- | --- |
| `discord_docs` | API reference from Discord's OpenAPI spec, updated daily. |
| `discord_get` | GET requests: servers, channels, messages, members, roles. |
| `discord_call` | POST, PUT, PATCH, DELETE: send, edit, moderate, upload. |
| `discord_events` | Gateway events as they arrive: messages, reactions, joins. |
| `discord_gateway` | The bot's status and activity, and member lookups. |

## Example

Asked to open a thread for release notes and wait for a reply.

```
> discord_docs {
    "keywords": ["thread", "threads"]
  }
POST /channels/{channel_id}/threads — create_thread
…

> discord_call {
    "method": "POST",
    "path": "/channels/1290…/threads",
    "body": { "name": "Release notes" },
    "jq": ".id"
  }
"1301…"

> discord_events {
    "types": ["MESSAGE_CREATE"],
    "wait": 300,
    "jq": "{author: .author.username, content}"
  }
#815 MESSAGE_CREATE {"author":"ann","content":"looks good"}
```

## Options

| Flag | |
| --- | --- |
| `--read-only` | Nothing on Discord can be changed: no `discord_call` or `discord_gateway`. |
| `--disable-gateway` | REST only: no events, and the bot stays offline. |
| `--intents <list>` | Gateway intents, e.g. `guilds,guild_messages,message_content`. Default: everything but typing and presences, plus the privileged ones switched on for the bot. |
| `--no-hub` | Keep the gateway in this process instead of sharing it. |
| `--allow-unlisted` | Allow paths the API reference doesn't list. |
| `--no-spec-update` | Use the built-in API reference only. |
| `--max-output <chars>` | Longest answer of one call (default 40000). |

`discord-mcp call <tool> '<json>'` runs one call from a shell.

## Several agents, one bot

A bot should have one gateway connection, but each agent starts its own server. The first one starts a
background hub that keeps the connection, the event buffer and the rate limits; the others connect to it over
loopback, with a secret only you can read. Each reads events by its own cursor without taking them from the
others, and the hub leaves a minute after the last one.

## Safety

- The agent can do whatever the bot can. Give the bot only the permissions you want used, or run `--read-only`.
- Discord content is written by other people and may contain instructions aimed at the agent. Keep
  `discord_call` behind your client's approval prompt.
- `discord_call` uploads the local files it is given paths to.
- The token goes to Discord only: it is never printed, and `jq` can't read the environment.

Not supported: voice, and sharding (bots in 2500+ servers).

## Development

```sh
cargo test
```

The tests run the real binary against a stand-in Discord REST API and gateway; no token needed.

Releases are made by [release-plz](https://release-plz.dev). It keeps a pull request open with the next
version and changelog, told from [conventional commits](https://www.conventionalcommits.org) (`feat:`,
`fix:`, `feat!:` for a breaking change); merging it publishes to crates.io and builds the binaries.

## License

GPL-3.0-or-later. The bundled `spec/openapi.json` is Discord's, under the MIT license in `spec/LICENSE`.
Not affiliated with Discord.
