//! One OpenRouter request that names a tab holding several agents.
//!
//! Runs `curl` (no HTTP dependency) on a worker thread. The key is read from
//! `OPENROUTER_API_KEY` or an env file and handed to curl through a private
//! temporary config file, never through argv.

use crate::proc;
use crate::pyjson::{self, Map, Value};
use std::path::{Path, PathBuf};

const SYSTEM: &str = "You name terminal tabs that hold several AI coding agent sessions. \
Reply with ONLY the tab name: 2-4 words, at most 28 characters, no quotes, no trailing punctuation.";

/// The key from the environment, else the last matching line of `env_file`.
pub fn api_key(env_file: &str) -> Option<String> {
    if let Some(key) = std::env::var("OPENROUTER_API_KEY").ok().filter(|k| !k.trim().is_empty()) {
        return Some(key.trim().to_string());
    }
    let path = if let Some(rest) = env_file.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else {
        PathBuf::from(env_file)
    };
    if env_file.is_empty() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("export ").or(Some(line.trim())))
        .filter_map(|line| line.strip_prefix("OPENROUTER_API_KEY="))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|v| !v.is_empty())
        .last()
}

fn request_body(model: &str, titles: &[String]) -> String {
    let list: String = titles.iter().map(|t| format!("- {}\n", t)).collect();
    let mut system = Map::new();
    system.insert("role", Value::str("system"));
    system.insert("content", Value::str(SYSTEM));
    let mut user = Map::new();
    user.insert("role", Value::str("user"));
    user.insert("content", Value::str(format!("Session titles:\n{}", list)));
    let mut body = Map::new();
    body.insert("model", Value::str(model));
    body.insert("max_tokens", Value::Int(600));
    body.insert("temperature", Value::Float(0.2));
    body.insert("messages", Value::List(vec![Value::Dict(system), Value::Dict(user)]));
    pyjson::dumps(&Value::Dict(body), false)
}

/// The reply's text, or None for any failure.
pub fn parse_reply(text: &str) -> Option<String> {
    let value = pyjson::loads(text).ok()?;
    let content = value.get("choices").list_or_empty().first()?.get("message").get("content").as_str()?.to_string();
    let line = content.lines().map(str::trim).find(|l| !l.is_empty())?;
    let name = line.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '.').trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// Ask `model` for a name; blocks up to ~25 s.
pub fn name_tab(key: &str, model: &str, titles: &[String], scratch: &Path) -> Option<String> {
    std::fs::create_dir_all(scratch).ok()?;
    let config = scratch.join(format!("llm-{}.curl", std::process::id()));
    let header = format!("header = \"Authorization: Bearer {}\"\n", key.replace('\\', "\\\\").replace('"', "\\\""));
    write_private(&config, &header).ok()?;
    let argv: Vec<String> = [
        "curl",
        "-sS",
        "--max-time",
        "20",
        "-K",
        &config.to_string_lossy(),
        "-H",
        "Content-Type: application/json",
        "--data-binary",
        &request_body(model, titles),
        "https://openrouter.ai/api/v1/chat/completions",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = proc::run(&argv, 25.0);
    let _ = std::fs::remove_file(&config);
    let out = out.ok()?;
    if !out.success {
        return None;
    }
    parse_reply(&String::from_utf8(out.stdout).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_is_cleaned() {
        let reply = r#"{"choices":[{"message":{"content":"\n\"World Clock Tab Fix.\"\nextra"}}]}"#;
        assert_eq!(parse_reply(reply).as_deref(), Some("World Clock Tab Fix"));
        assert_eq!(parse_reply(r#"{"error":{"message":"no"}}"#), None);
        assert_eq!(parse_reply(r#"{"choices":[{"message":{"content":""}}]}"#), None);
    }

    #[test]
    fn body_lists_every_title() {
        let body = request_body("m/x", &["a b".into(), "c \"d\"".into()]);
        let value = pyjson::loads(&body).unwrap();
        let user = value.get("messages").list_or_empty()[1].get("content").as_str().unwrap().to_string();
        assert_eq!(user, "Session titles:\n- a b\n- c \"d\"\n");
        assert_eq!(value.get("model").as_str(), Some("m/x"));
    }

    #[test]
    fn key_comes_from_env_file() {
        let dir = std::env::temp_dir().join(format!("llm-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("env");
        std::fs::write(&file, "A=1\nOPENROUTER_API_KEY=\"sk-test\"\n").unwrap();
        if std::env::var_os("OPENROUTER_API_KEY").is_none() {
            assert_eq!(api_key(&file.to_string_lossy()).as_deref(), Some("sk-test"));
        }
        assert_eq!(api_key(""), std::env::var("OPENROUTER_API_KEY").ok().filter(|k| !k.trim().is_empty()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
