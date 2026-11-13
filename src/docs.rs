//! What discord_docs answers: the overview, endpoint search, one endpoint
//! in full, one named type.

use serde_json::Value;

use crate::schema::{self, ref_name, type_expr};
use crate::spec::{Operation, Route, Spec, templates};

const SEARCH_LIMIT: usize = 20;

const OVERVIEW: &str = r#"This tool:
  {"query": "ban member"}           find endpoints by keywords
  {"operation": "create_message"}   one endpoint in full: parameters, body fields, what it returns
                                    (also as "POST /channels/{channel_id}/messages")
  {"schema": "RichEmbed"}           the fields of a named type

Common calls
  the bot itself           GET    /users/@me
  servers the bot is in    GET    /users/@me/guilds
  a server's channels      GET    /guilds/{guild_id}/channels
  a server's roles         GET    /guilds/{guild_id}/roles
  members                  GET    /guilds/{guild_id}/members            query limit (up to 1000), after
  find a member by name    GET    /guilds/{guild_id}/members/search     query query, limit
  recent messages          GET    /channels/{channel_id}/messages       query limit (up to 100), before, after
  send a message           POST   /channels/{channel_id}/messages       {"content": "…"}
  reply                    the same, plus "message_reference": {"message_id": "…"}
  edit, delete a message   PATCH, DELETE /channels/{channel_id}/messages/{message_id}
  react                    PUT    /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me
  open a DM                POST   /users/@me/channels                   {"recipient_id": "…"}, then send to its id
  start a thread           POST   /channels/{channel_id}/threads, or …/messages/{message_id}/threads
  give, take a role        PUT, DELETE /guilds/{guild_id}/members/{user_id}/roles/{role_id}
  kick                     DELETE /guilds/{guild_id}/members/{user_id}
  ban, unban               PUT, DELETE /guilds/{guild_id}/bans/{user_id}
  time out                 PATCH  /guilds/{guild_id}/members/{user_id}  {"communication_disabled_until": "<ISO 8601 time>"}
  audit log                GET    /guilds/{guild_id}/audit-logs

Conventions
- Ids are snowflakes: strings of digits. A thread is a channel, and so is a DM.
- Lists page with limit and before/after (an id). Messages come newest first.
- In message text <@user_id> mentions a user, <@&role_id> a role, <#channel_id> a channel; <t:unix_seconds> shows a time.
- A bot's message content holds up to 2000 characters, an embed description 4096.
- The {emoji} of a reaction path is the character itself (👍), or name:id for a custom emoji.
- Permissions are a bitfield written as a decimal string.
- Listing members needs the Server Members intent, and reading what other people wrote needs the Message Content
  intent; both are switched on in the Developer Portal, on the application's Bot page. Messages that come back with
  an empty "content" usually mean the second one is off.
- Bulk delete takes 2 to 100 messages younger than 14 days.
- reason on discord_call is written to the server's audit log.

Events (discord_events, when this server has it)
  Each event's data is the object REST returns for the same thing: a message, a member, a channel.
  messages        MESSAGE_CREATE, MESSAGE_UPDATE, MESSAGE_DELETE, MESSAGE_DELETE_BULK
  reactions       MESSAGE_REACTION_ADD, MESSAGE_REACTION_REMOVE, MESSAGE_REACTION_REMOVE_ALL
  members         GUILD_MEMBER_ADD, GUILD_MEMBER_UPDATE, GUILD_MEMBER_REMOVE   (needs the Server Members intent)
  moderation      GUILD_BAN_ADD, GUILD_BAN_REMOVE, GUILD_AUDIT_LOG_ENTRY_CREATE, AUTO_MODERATION_ACTION_EXECUTION
  channels        CHANNEL_CREATE, CHANNEL_UPDATE, CHANNEL_DELETE, CHANNEL_PINS_UPDATE
  threads         THREAD_CREATE, THREAD_UPDATE, THREAD_DELETE, THREAD_MEMBERS_UPDATE
  roles           GUILD_ROLE_CREATE, GUILD_ROLE_UPDATE, GUILD_ROLE_DELETE
  servers         GUILD_CREATE, GUILD_UPDATE, GUILD_DELETE
  voice           VOICE_STATE_UPDATE
  invites         INVITE_CREATE, INVITE_DELETE
  scheduled       GUILD_SCHEDULED_EVENT_CREATE, GUILD_SCHEDULED_EVENT_UPDATE, GUILD_SCHEDULED_EVENT_DELETE
  polls           MESSAGE_POLL_VOTE_ADD, MESSAGE_POLL_VOTE_REMOVE
  interactions    INTERACTION_CREATE (a slash command or button; it must be answered within 3 seconds)
  the connection  READY, RESUMED
  Typing and presence events are off unless this server was started with them in --intents."#;

pub fn overview(spec: &Spec) -> String {
    format!(
        "Discord REST API v10: {} endpoints, reached with discord_get (GET) and discord_call (POST, PUT, PATCH, \
         DELETE).\n\n{OVERVIEW}",
        spec.ops.len()
    )
}

fn line(op: &Operation) -> String {
    match &op.description {
        Some(d) => format!("  {} {} — {}: {d}", op.method, op.path, op.id),
        None => format!("  {} {} — {}", op.method, op.path, op.id),
    }
}

pub fn search(spec: &Spec, query: &str) -> String {
    let found = spec.search(query, SEARCH_LIMIT);
    let types = spec.search_schemas(query, 8);
    let mut out = String::new();
    if found.hits.is_empty() {
        out.push_str(&format!(
            "No endpoint matches \"{query}\". Try the words Discord's API uses (guild for server, create/update/delete, \
             member, role, channel, message, thread, webhook, invite, emoji, command)."
        ));
    } else {
        if found.matched < found.words {
            out.push_str(&format!("No endpoint matches every word of \"{query}\". Closest:\n"));
        } else {
            out.push_str(&format!("Endpoints matching \"{query}\":\n"));
        }
        out.push_str(&found.hits.iter().map(|op| line(op)).collect::<Vec<_>>().join("\n"));
        out.push_str("\nRead one in full with {\"operation\": \"<name>\"}.");
    }
    if !types.is_empty() {
        out.push_str(&format!(
            "\nTypes: {} (read one with {{\"schema\": \"<name>\"}}).",
            types.join(", ")
        ));
    }
    out
}

/// One endpoint in full, looked up by name, by `METHOD /path`, or by path alone (every method on it).
pub fn operation(spec: &Spec, what: &str) -> Result<String, String> {
    let what = what.trim();
    let (method, path) = match what.split_once(char::is_whitespace) {
        Some((method, path)) => (Some(method.to_ascii_uppercase()), path.trim()),
        None if what.starts_with('/') => (None, what),
        None => {
            return match spec.by_id(what) {
                Some(op) => Ok(render_operation(spec, op)),
                None => Err(format!(
                    "No endpoint is named {what}.\n{}",
                    search(spec, &what.replace('_', " "))
                )),
            };
        }
    };
    let path = path.split('?').next().unwrap_or(path).trim_end_matches('/');
    // A template's `{channel_id}` matches the parameter it stands for, so templates and real paths both work here.
    let on_path = match spec.route(method.as_deref().unwrap_or("GET"), path) {
        Route::Found(op) if method.is_some() => return Ok(render_operation(spec, op)),
        Route::Found(op) => spec.on_template(&op.path),
        Route::WrongMethod {
            path: template,
            allowed,
        } => match &method {
            Some(method) => {
                return Err(format!(
                    "{template} doesn't take {method}; it takes {}.",
                    allowed.join(", ")
                ));
            }
            None => spec.on_template(template),
        },
        Route::Unknown { nearest } => {
            let mut message = format!("No endpoint has the path {path}.");
            if !nearest.is_empty() {
                message.push_str(&format!(
                    "\nClosest:\n{}",
                    nearest.iter().map(|op| line(op)).collect::<Vec<_>>().join("\n")
                ));
            }
            return Err(message);
        }
    };
    Ok(on_path
        .into_iter()
        .map(|op| render_operation(spec, op))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

pub fn schema(spec: &Spec, name: &str) -> Result<String, String> {
    let name = name.trim();
    match spec.schema(name) {
        Some((name, schema)) => Ok(schema::render_named(spec, name, schema)),
        None => {
            let close = spec.search_schemas(name, 12);
            Err(if close.is_empty() {
                format!("No type is named {name}. Type names appear in an endpoint's reference: {{\"operation\": …}}.")
            } else {
                format!("No type is named {name}. Close: {}.", close.join(", "))
            })
        }
    }
}

pub fn render_operation(spec: &Spec, op: &Operation) -> String {
    let object = op.object(spec);
    let mut out = format!("{} {} — {}", op.method, op.path, op.id);
    if let Some(d) = &op.description {
        out.push_str(&format!("\n{d}"));
    }
    for (place, label) in [("path", "path"), ("query", "query")] {
        let lines: Vec<String> = op
            .parameters(spec)
            .into_iter()
            .filter(|p| p["in"] == place)
            .map(|p| parameter_line(spec, p))
            .collect();
        if !lines.is_empty() {
            out.push_str(&format!("\n{label}:\n{}", lines.join("\n")));
        }
    }
    let content = &object["requestBody"]["content"];
    let json = &content["application/json"]["schema"];
    let multipart = &content["multipart/form-data"]["schema"];
    let body = if json.is_object() { json } else { multipart };
    if body.is_object() {
        let name = body["$ref"]
            .as_str()
            .map(|r| format!(" {}", ref_name(r)))
            .unwrap_or_default();
        let lines = schema::field_lines(spec, body, "  ");
        if lines.is_empty() {
            out.push_str(&format!("\nbody:{name} {}", type_expr(spec, body)));
        } else {
            out.push_str(&format!("\nbody:{name}\n{}", lines.join("\n")));
        }
    }
    if multipart.is_object() {
        let files = schema::file_fields(spec, multipart);
        if !files.is_empty() {
            out.push_str(&format!(
                "\nfiles: takes uploads; give local paths in discord_call's files (up to {}).",
                files.len()
            ));
        }
    }
    out.push_str(&format!("\nreturns: {}", returns(spec, object)));
    out
}

fn parameter_line(spec: &Spec, parameter: &Value) -> String {
    let schema = &parameter["schema"];
    let optional = if parameter["required"] == true { "" } else { "?" };
    let description = match parameter["description"].as_str() {
        Some(d) if !d.trim().is_empty() => format!(" — {}", d.trim()),
        _ => String::new(),
    };
    let range = match (&schema["minimum"], &schema["maximum"]) {
        (Value::Number(min), Value::Number(max)) => format!(" ({min}..{max})"),
        (_, Value::Number(max)) => format!(" (max {max})"),
        (Value::Number(min), _) => format!(" (min {min})"),
        _ => String::new(),
    };
    format!(
        "  {}{optional}: {}{range}{description}",
        parameter["name"].as_str().unwrap_or("?"),
        type_expr(spec, schema)
    )
}

fn returns(spec: &Spec, operation: &Value) -> String {
    let Some(responses) = operation["responses"].as_object() else {
        return "unknown".into();
    };
    let mut kinds: Vec<String> = Vec::new();
    for (status, response) in responses {
        if !status.starts_with('2') {
            continue;
        }
        let schema = &response["content"]["application/json"]["schema"];
        let kind = if schema.is_object() {
            type_expr(spec, schema)
        } else if let Some(other) = response["content"].as_object().and_then(|c| c.keys().next()) {
            format!("{other} ({status})")
        } else {
            format!("nothing ({status})")
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    if kinds.is_empty() {
        "unknown".into()
    } else {
        kinds.join(" or ")
    }
}

/// What to tell an agent whose request reached no endpoint.
pub fn unknown_route(method: &str, path: &str, nearest: &[&Operation]) -> String {
    let mut out = format!("No Discord endpoint is {method} {path}.");
    let near = templates(nearest.iter().copied());
    if !near.is_empty() {
        out.push_str(&format!("\nPaths that start the same: {}", near.join(", ")));
    }
    out.push_str("\nFind the right one with discord_docs {\"query\": \"…\"}.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::EMBEDDED;

    #[test]
    fn operation_reference_has_parameters_body_and_result() {
        let text = operation(&EMBEDDED, "create_message").unwrap();
        assert!(
            text.starts_with("POST /channels/{channel_id}/messages — create_message"),
            "{text}"
        );
        assert!(text.contains("path:\n  channel_id: snowflake"), "{text}");
        assert!(text.contains("body: MessageCreateRequest\n"), "{text}");
        assert!(text.contains("content?: string | null (max 4000 chars)"), "{text}");
        assert!(text.contains("files: takes uploads"), "{text}");
        assert!(text.contains("returns: MessageResponse"), "{text}");
    }

    #[test]
    fn operation_is_found_by_method_and_path() {
        let by_template = operation(&EMBEDDED, "GET /channels/{channel_id}/messages").unwrap();
        assert!(by_template.contains("list_messages"));
        assert!(by_template.contains("limit?: integer (1..100)"), "{by_template}");
        let by_real_path = operation(&EMBEDDED, "get /channels/123/messages").unwrap();
        assert_eq!(by_template, by_real_path);
    }

    #[test]
    fn a_bare_path_shows_every_method() {
        let text = operation(&EMBEDDED, "/users/@me").unwrap();
        assert!(
            text.contains("get_my_user") && text.contains("update_my_user"),
            "{text}"
        );
    }

    #[test]
    fn misses_point_somewhere() {
        assert!(
            operation(&EMBEDDED, "POST /users/@me")
                .unwrap_err()
                .contains("GET, PATCH")
        );
        assert!(
            operation(&EMBEDDED, "send_message")
                .unwrap_err()
                .contains("create_message")
        );
        assert!(schema(&EMBEDDED, "embed").unwrap_err().contains("RichEmbed"));
    }

    #[test]
    fn no_content_endpoints_say_so() {
        let text = operation(&EMBEDDED, "delete_message").unwrap();
        assert!(text.contains("returns: nothing (204)"), "{text}");
    }

    #[test]
    fn search_lists_endpoints_and_types() {
        let text = search(&EMBEDDED, "ban");
        assert!(
            text.contains("PUT /guilds/{guild_id}/bans/{user_id} — ban_user_from_guild"),
            "{text}"
        );
        assert!(text.contains("Types:"), "{text}");
    }
}
