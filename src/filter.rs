//! Shaping a response before an agent reads it: a jq filter, and a cap on
//! how much text comes back.

use std::time::{Duration, Instant};

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data};
use jaq_json::Val;
use serde::Deserialize;
use serde_json::Value;

/// Longest a filter may keep producing output.
pub const JQ_TIMEOUT: Duration = Duration::from_secs(5);

/// `env` would hand the bot token to whoever writes the filter.
const WITHHELD: [&str; 1] = ["env"];

pub fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!("(unprintable JSON: {e})"))
}

/// A JSON value as plain text: a string as it is, anything else as compact JSON. What a query
/// parameter or a form field holds.
pub fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A jq filter, compiled once and run on as many values as needed.
pub struct Program(jaq_core::Filter<data::JustLut<Val>>);

impl Program {
    pub fn compile(filter: &str) -> Result<Self, String> {
        let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
        let funs = jaq_core::funs()
            .chain(jaq_std::funs())
            .chain(jaq_json::funs())
            .filter(|(name, _, _)| !WITHHELD.contains(name));
        let loader = Loader::new(defs);
        let arena = Arena::default();
        let modules = loader.load(&arena, File { code: filter, path: () }).map_err(|errors| {
            let mut problems = Vec::new();
            for (_, error) in errors {
                match error {
                    jaq_core::load::Error::Io(list) => {
                        problems.extend(list.into_iter().map(|(path, e)| format!("can't load {path}: {e}")))
                    }
                    jaq_core::load::Error::Lex(list) => problems.extend(
                        list.into_iter()
                            .map(|(expected, at)| format!("expected {} at `{}`", expected.as_str(), start(at))),
                    ),
                    jaq_core::load::Error::Parse(list) => problems.extend(
                        list.into_iter()
                            .map(|(expected, at)| format!("expected {} at `{}`", expected.as_str(), start(at))),
                    ),
                }
            }
            format!("jq filter doesn't parse: {}", problems.join("; "))
        })?;
        let compiled = Compiler::default().with_funs(funs).compile(modules).map_err(|errors| {
            let problems: Vec<String> = errors
                .into_iter()
                .flat_map(|(_, list)| list)
                .map(|(name, kind)| format!("undefined {} `{name}`", kind.as_str()))
                .collect();
            format!("jq filter doesn't compile: {}", problems.join("; "))
        })?;
        Ok(Self(compiled))
    }

    /// Each output on its own line: strings as they are, everything else as compact JSON. Stops once
    /// `max_chars` is passed, since the caller cuts there anyway.
    pub fn run(&self, input: &Value, max_chars: usize) -> Result<String, String> {
        let input = Val::deserialize(input).map_err(|e| format!("response isn't JSON jq can read: {e}"))?;
        let ctx = Ctx::<data::JustLut<Val>>::new(&self.0.lut, Vars::new([]));
        let deadline = Instant::now() + JQ_TIMEOUT;
        let mut out = String::new();
        for result in self.0.id.run((ctx, input)) {
            let value = match result {
                Ok(value) => value,
                Err(exception) => {
                    return Err(match exception.get_err() {
                        Ok(error) => format!("jq filter failed: {error}"),
                        // `halt` would end a jq process; here it only ends the filter.
                        Err(_) => "jq filter stopped itself with halt".into(),
                    });
                }
            };
            if !out.is_empty() {
                out.push('\n');
            }
            match &value {
                Val::TStr(text) | Val::BStr(text) => out.push_str(&String::from_utf8_lossy(text)),
                other => out.push_str(&other.to_string()),
            }
            if out.len() > max_chars {
                break;
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "jq filter ran longer than {} s; it was stopped",
                    JQ_TIMEOUT.as_secs()
                ));
            }
        }
        Ok(out)
    }

    /// Does the filter, as a condition, hold for `input`? Its first output decides: anything but
    /// `false` and `null`. No output, or an error (a field that isn't there), is a no.
    pub fn holds(&self, input: &Value) -> bool {
        let Ok(input) = Val::deserialize(input) else {
            return false;
        };
        let ctx = Ctx::<data::JustLut<Val>>::new(&self.0.lut, Vars::new([]));
        matches!(self.0.id.run((ctx, input)).next(), Some(Ok(value)) if !matches!(value, Val::Null | Val::Bool(false)))
    }
}

/// Run a jq filter over `input` once.
pub fn jq(filter: &str, input: &Value, max_chars: usize) -> Result<String, String> {
    Program::compile(filter)?.run(input, max_chars)
}

fn start(text: &str) -> String {
    let short: String = text.chars().take(24).collect();
    if short.is_empty() { "the end".into() } else { short }
}

/// Cut `text` to `max_chars`, saying how much is missing and how to get less next time.
pub fn cap(text: String, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!(
        "{kept}\n… cut: this is the first {max_chars} of {total} characters. Pass jq to keep only the fields you \
         need (for a list: `.[] | {{id, name}}`), or ask for fewer items with limit."
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn filters_project_and_print_one_value_per_line() {
        let input = json!([{"id": "1", "name": "general", "x": 1}, {"id": "2", "name": "dev", "x": 2}]);
        assert_eq!(
            jq(".[] | {id, name}", &input, 10_000).unwrap(),
            "{\"id\":\"1\",\"name\":\"general\"}\n{\"id\":\"2\",\"name\":\"dev\"}"
        );
        assert_eq!(
            jq(".[] | \"\\(.id) \\(.name)\"", &input, 10_000).unwrap(),
            "1 general\n2 dev"
        );
        assert_eq!(jq("map(.x) | add", &input, 10_000).unwrap(), "3");
        assert_eq!(jq("length", &input, 10_000).unwrap(), "2");
    }

    #[test]
    fn snowflakes_keep_every_digit() {
        let input = json!({"id": "1234567890123456789", "n": 1234567890123456789u64});
        assert_eq!(jq(".id", &input, 100).unwrap(), "1234567890123456789");
        assert_eq!(jq(".n", &input, 100).unwrap(), "1234567890123456789");
    }

    #[test]
    fn the_environment_stays_out_of_reach() {
        let error = jq("env", &json!(null), 100).unwrap_err();
        assert!(error.contains("undefined filter `env`"), "{error}");
        assert!(jq("$ENV", &json!(null), 100).is_err());
    }

    #[test]
    fn halt_ends_the_filter_not_the_process() {
        let error = jq("halt", &json!(null), 100).unwrap_err();
        assert!(error.contains("halt"), "{error}");
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(jq(".[", &json!(null), 100).unwrap_err().contains("doesn't parse"));
        assert!(
            jq("nope", &json!(null), 100)
                .unwrap_err()
                .contains("undefined filter `nope`")
        );
        assert!(jq(".a.b", &json!([1]), 100).unwrap_err().contains("failed"));
    }

    #[test]
    fn endless_output_stops_at_the_cap() {
        let out = jq("repeat(1)", &json!(null), 50).unwrap();
        assert!(out.len() <= 60, "{}", out.len());
    }

    #[test]
    fn a_condition_holds_or_does_not() {
        let from_person = Program::compile(".channel_id == \"1\" and (.author.bot | not)").unwrap();
        assert!(from_person.holds(&json!({"channel_id": "1", "author": {"id": "5"}})));
        assert!(!from_person.holds(&json!({"channel_id": "1", "author": {"bot": true}})));
        assert!(!from_person.holds(&json!({"channel_id": "2", "author": {"id": "5"}})));
        // An event without those fields simply doesn't match.
        assert!(!from_person.holds(&json!("just a string")));
        assert!(!Program::compile("empty").unwrap().holds(&json!(1)));
    }

    #[test]
    fn long_text_is_cut_with_a_note() {
        assert_eq!(cap("short".into(), 10), "short");
        let cut = cap("x".repeat(100), 10);
        assert!(
            cut.starts_with("xxxxxxxxxx\n… cut: this is the first 10 of 100 characters"),
            "{cut}"
        );
    }
}
