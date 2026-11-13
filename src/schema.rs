//! JSON Schema from the spec, written the way a type is read: one field
//! per line, `name?: type (limits) — description`.

use serde_json::Value;

use crate::spec::Spec;

/// How deep fields of objects written inline (not a named type) are listed.
const MAX_DEPTH: usize = 3;
/// Longest description kept on a field's line.
const MAX_DESCRIPTION: usize = 160;
/// Most enum values written out in a type.
const MAX_INLINE_ENUM: usize = 12;

pub fn ref_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

/// A schema as a type on one line: `string | null`, `RichEmbed[]`, `0 | 1 | 2`.
pub fn type_expr(spec: &Spec, schema: &Value) -> String {
    if let Some(reference) = schema["$ref"].as_str() {
        // A named scalar (SnowflakeType) reads better as what it is.
        return match spec.resolve(reference).and_then(scalar) {
            Some(scalar) => scalar,
            None => ref_name(reference).to_owned(),
        };
    }
    if let Some(value) = schema.get("const") {
        return value.to_string();
    }
    if let Some(values) = schema["enum"].as_array() {
        return literals(values.iter());
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(options) = schema[key].as_array() {
            if !options.is_empty() && options.iter().all(|o| o.get("const").is_some()) {
                return literals(options.iter().map(|o| &o["const"]));
            }
            let mut parts: Vec<String> = Vec::new();
            for option in options {
                let part = type_expr(spec, option);
                if !parts.contains(&part) {
                    parts.push(part);
                }
            }
            // `null` reads best last.
            parts.sort_by_key(|p| p == "null");
            return parts.join(" | ");
        }
    }
    if let Some(parts) = schema["allOf"].as_array() {
        return parts.iter().map(|p| type_expr(spec, p)).collect::<Vec<_>>().join(" & ");
    }
    match &schema["type"] {
        Value::String(t) => base_type(spec, t, schema),
        Value::Array(types) => {
            let mut parts: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .map(|t| base_type(spec, t, schema))
                .collect();
            parts.sort_by_key(|p| p == "null");
            parts.join(" | ")
        }
        _ if schema.get("properties").is_some() => "object".into(),
        _ => "any".into(),
    }
}

fn literals<'a>(values: impl Iterator<Item = &'a Value>) -> String {
    let all: Vec<String> = values.map(Value::to_string).collect();
    if all.len() > MAX_INLINE_ENUM {
        format!("{} | … ({} values)", all[..MAX_INLINE_ENUM].join(" | "), all.len())
    } else {
        all.join(" | ")
    }
}

fn base_type(spec: &Spec, name: &str, schema: &Value) -> String {
    match name {
        "array" => {
            let item = schema.get("items").map(|i| type_expr(spec, i)).unwrap_or("any".into());
            if item.contains(" | ") || item.contains(" & ") {
                format!("({item})[]")
            } else {
                format!("{item}[]")
            }
        }
        "object" => match schema.get("additionalProperties") {
            _ if schema.get("properties").is_some() => "object".into(),
            Some(values @ Value::Object(_)) => format!("{{[key]: {}}}", type_expr(spec, values)),
            _ => "object".into(),
        },
        "string" => string_type(schema),
        other => other.to_owned(),
    }
}

/// A string by its format: `snowflake`, `string<date-time>`, or plain `string`.
fn string_type(schema: &Value) -> String {
    match schema["format"].as_str() {
        Some("snowflake") => "snowflake".into(),
        Some(format) => format!("string<{format}>"),
        None => "string".into(),
    }
}

/// The type of a schema that is one plain value: no fields, no fixed set of values.
fn scalar(schema: &Value) -> Option<String> {
    let name = schema["type"].as_str()?;
    let plain = ["enum", "oneOf", "anyOf", "allOf", "const", "properties"]
        .iter()
        .all(|k| schema.get(k).is_none());
    (plain && ["string", "integer", "number", "boolean"].contains(&name)).then(|| match name {
        "string" => string_type(schema),
        other => other.to_owned(),
    })
}

/// Length, range and size limits: ` (max 4000 chars)`, ` (1..100)`.
fn limits(schema: &Value) -> String {
    let int = |key: &str| schema[key].as_i64();
    let parts: Vec<String> = [
        count_limit(int("minLength"), int("maxLength"), "chars"),
        range(schema),
        count_limit(int("minItems"), int("maxItems"), "items"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// The bounds of a number: `1..100`, `min 0`, `max 25`.
pub fn range(schema: &Value) -> Option<String> {
    match (&schema["minimum"], &schema["maximum"]) {
        (Value::Number(min), Value::Number(max)) => Some(format!("{min}..{max}")),
        (Value::Number(min), _) => Some(format!("min {min}")),
        (_, Value::Number(max)) => Some(format!("max {max}")),
        _ => None,
    }
}

/// The bounds of a length or a size, in `unit`s: `1..100 items`, `max 4000 chars`. A minimum of 0
/// says nothing.
fn count_limit(min: Option<i64>, max: Option<i64>, unit: &str) -> Option<String> {
    match (min.filter(|&min| min > 0), max) {
        (Some(min), Some(max)) => Some(format!("{min}..{max} {unit}")),
        (None, Some(max)) => Some(format!("max {max} {unit}")),
        (Some(min), None) => Some(format!("min {min} {unit}")),
        (None, None) => None,
    }
}

fn description(schema: &Value) -> String {
    match schema["description"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() => {
            let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            format!(" — {}", shorten(&one_line, MAX_DESCRIPTION))
        }
        _ => String::new(),
    }
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

struct Field<'a> {
    name: &'a str,
    schema: &'a Value,
    required: bool,
}

/// The fields of an object schema, through `$ref` and `allOf`.
fn fields<'a>(spec: &'a Spec, schema: &'a Value, out: &mut Vec<Field<'a>>, depth: usize) {
    if depth > 8 {
        return;
    }
    if let Some(target) = schema["$ref"].as_str().and_then(|r| spec.resolve(r)) {
        return fields(spec, target, out, depth + 1);
    }
    if let Some(parts) = schema["allOf"].as_array() {
        for part in parts {
            fields(spec, part, out, depth + 1);
        }
    }
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if let Some(properties) = schema["properties"].as_object() {
        for (name, property) in properties {
            if !out.iter().any(|f| f.name == name) {
                out.push(Field {
                    name,
                    schema: property,
                    required: required.contains(&name.as_str()),
                });
            }
        }
    }
}

/// Is this field an uploaded file rather than JSON?
fn is_binary(schema: &Value) -> bool {
    schema["contentEncoding"] == "binary" || schema["format"] == "binary"
}

/// The multipart field names that carry files, in order (`files[0]`, … or `file`).
pub fn file_fields(spec: &Spec, schema: &Value) -> Vec<String> {
    let mut all = Vec::new();
    fields(spec, schema, &mut all, 0);
    all.iter()
        .filter(|f| is_binary(f.schema))
        .map(|f| f.name.to_owned())
        .collect()
}

/// One line per field of an object schema, files left out.
pub fn field_lines(spec: &Spec, schema: &Value, indent: &str) -> Vec<String> {
    let mut lines = Vec::new();
    write_fields(spec, schema, indent, 0, &mut lines);
    lines
}

fn write_fields(spec: &Spec, schema: &Value, indent: &str, depth: usize, lines: &mut Vec<String>) {
    let mut all = Vec::new();
    fields(spec, schema, &mut all, 0);
    for field in all.iter().filter(|f| !is_binary(f.schema)) {
        let optional = if field.required { "" } else { "?" };
        lines.push(format!(
            "{indent}{}{optional}: {}{}{}",
            field.name,
            type_expr(spec, field.schema),
            limits(field.schema),
            description(field.schema),
        ));
        // An object written in place has no name to look up, so its fields go right under it.
        let inline = [field.schema, &field.schema["items"]]
            .into_iter()
            .find(|s| s.get("properties").is_some());
        if let Some(inline) = inline
            && depth + 1 < MAX_DEPTH
        {
            write_fields(spec, inline, &format!("{indent}  "), depth + 1, lines);
        }
    }
}

/// A named schema in full: its fields, its values, or what it is one of.
pub fn render_named(spec: &Spec, name: &str, schema: &Value) -> String {
    let mut out = format!("{name}{}", description(schema));
    for key in ["oneOf", "anyOf"] {
        let Some(options) = schema[key].as_array() else {
            continue;
        };
        if !options.is_empty() && options.iter().all(|o| o.get("const").is_some()) {
            let kind = schema["type"].as_str().unwrap_or("value");
            out.push_str(&format!("\n{kind}, one of:"));
            for option in options {
                let title = option["title"].as_str().unwrap_or_default();
                out.push_str(&format!("\n  {}  {title}{}", option["const"], description(option)));
            }
        } else {
            out.push_str("\none of:");
            for option in options {
                out.push_str(&format!("\n  {}", type_expr(spec, option)));
            }
        }
        return out;
    }
    if let Some(values) = schema["enum"].as_array() {
        let all: Vec<String> = values.iter().map(Value::to_string).collect();
        out.push_str(&format!("\none of: {}", all.join(" | ")));
        return out;
    }
    let lines = field_lines(spec, schema, "  ");
    if lines.is_empty() {
        out.push_str(&format!("\n{}{}", type_expr(spec, schema), limits(schema)));
    } else {
        out.push_str("\nfields:\n");
        out.push_str(&lines.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::spec::FIXTURE;

    #[test]
    fn types_read_naturally() {
        let t = |v: Value| type_expr(&FIXTURE, &v);
        assert_eq!(t(json!({"type": ["string", "null"]})), "string | null");
        assert_eq!(t(json!({"$ref": "#/components/schemas/SnowflakeType"})), "snowflake");
        assert_eq!(t(json!({"$ref": "#/components/schemas/RichEmbed"})), "RichEmbed");
        assert_eq!(
            t(json!({"type": ["array", "null"], "items": {"$ref": "#/components/schemas/RichEmbed"}})),
            "RichEmbed[] | null"
        );
        assert_eq!(
            t(json!({"oneOf": [{"type": "null"}, {"$ref": "#/components/schemas/RichEmbed"}]})),
            "RichEmbed | null"
        );
        assert_eq!(
            t(json!({"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "integer"}]}})),
            "(string | integer)[]"
        );
        assert_eq!(t(json!({"oneOf": [{"const": 0}, {"const": 1}]})), "0 | 1");
        assert_eq!(t(json!({"enum": ["a", "b"]})), "\"a\" | \"b\"");
    }

    #[test]
    fn message_body_lists_its_fields() {
        let (name, schema) = FIXTURE.schema("MessageCreateRequest").unwrap();
        let text = render_named(&FIXTURE, name, schema);
        assert!(text.contains("content?: string | null (max 4000 chars)"), "{text}");
        assert!(text.contains("embeds?: RichEmbed[] | null (max 10 items)"), "{text}");
    }

    #[test]
    fn enums_list_their_values() {
        let (name, schema) = FIXTURE.schema("ChannelTypes").unwrap();
        let text = render_named(&FIXTURE, name, schema);
        assert!(
            text.contains("0  GUILD_TEXT — A text channel within a server"),
            "{text}"
        );
    }

    #[test]
    fn upload_fields_come_from_the_spec() {
        let op = FIXTURE.by_id("create_message").unwrap();
        let multipart = &op.object(&FIXTURE)["requestBody"]["content"]["multipart/form-data"]["schema"];
        let names = file_fields(&FIXTURE, multipart);
        assert_eq!(names.first().map(String::as_str), Some("files[0]"));
        assert_eq!(names.len(), 10);
    }
}
