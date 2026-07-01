//! Claude Code session source.
//!
//! Claude stores sessions as JSONL under `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`.
//! Each line is a JSON object. Conversation turns are `type: "user"|"assistant"` lines whose
//! `message.content` is either a plain string or an array of blocks (`text`, `thinking`,
//! `tool_use`, `tool_result`). Titles come from `type: "ai-title"` lines.
//!
//! Parsing is intentionally **loose**: unknown line types and malformed lines are skipped,
//! never fatal — the on-disk format drifts across releases.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{Block, Message, Provider, Role, Session, SessionSummary};

/// Default Claude config root.
const CLAUDE_DIR_NAME: &str = ".claude";

pub struct ClaudeSource {
    /// Root containing `projects/`. Defaults to `~/.claude`.
    pub root: PathBuf,
}

impl Default for ClaudeSource {
    fn default() -> Self {
        Self::new(default_root())
    }
}

fn default_root() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(CLAUDE_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(".").join(CLAUDE_DIR_NAME))
}

impl ClaudeSource {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    /// Recursively collect every `*.jsonl` under `projects/`.
    fn session_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let base = self.projects_dir();
        walk_jsonl(&base, &mut out);
        out.sort();
        out
    }
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            // `subagents/` holds sidechain transcripts: one file per spawned
            // subagent, each carrying the *parent* session's id. Including them
            // would list the same session id many times, so skip the directory.
            if path.file_name().and_then(|n| n.to_str()) == Some("subagents") {
                continue;
            }
            walk_jsonl(&path, out);
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

impl crate::source::SessionSource for ClaudeSource {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn discover(&self) -> Result<Vec<SessionSummary>> {
        let mut summaries = Vec::new();
        for file in self.session_files() {
            match summarize_file(&file) {
                Ok(Some(s)) => summaries.push(s),
                Ok(None) => {} // empty/uninteresting file
                Err(e) => eprintln!("warning: failed to read {}: {e}", file.display()),
            }
        }
        Ok(summaries)
    }

    fn load(&self, summary: &SessionSummary) -> Result<Session> {
        load_file(&summary.path).with_context(|| format!("loading {}", summary.path.display()))
    }
}

// ---- raw on-disk shapes (loose: everything optional) ----

#[derive(Deserialize, Default)]
struct RawLine {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(rename = "aiTitle", default)]
    ai_title: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawMessage {
    #[serde(default)]
    role: Option<String>,
    /// string OR array of blocks — parsed as raw Value, normalized later.
    #[serde(default)]
    content: Value,
}

#[derive(Deserialize, Default)]
struct RawBlock {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(rename = "tool_use_id", default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    is_error: Option<bool>,
}

// ---- parsing helpers ----

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Utc))
}

/// Read every line, returning parsed raw lines (skipping unparseable ones).
fn read_raw_lines(path: &Path) -> Result<Vec<RawLine>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<RawLine>(trimmed) {
            Ok(parsed) => out.push(parsed),
            Err(_) => continue, // skip malformed lines
        }
    }
    Ok(out)
}

/// Build a summary from a session file in a single pass.
fn summarize_file(path: &Path) -> Result<Option<SessionSummary>> {
    let lines = read_raw_lines(path)?;

    let mut id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut first_user_text: Option<String> = None;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;
    let mut message_count = 0usize;

    for line in &lines {
        if let Some(ts) = line.timestamp.as_deref().and_then(parse_ts) {
            updated_at = Some(ts);
            if started_at.is_none() {
                started_at = Some(ts);
            }
        }
        id = id.clone().or_else(|| line.session_id.clone());
        cwd = cwd.clone().or_else(|| line.cwd.clone());

        if let Some(title) = &line.ai_title {
            ai_title = Some(title.clone());
        }

        let Some(kind) = line.kind.as_deref() else { continue };
        if kind == "user" || kind == "assistant" {
            message_count += 1;
            if first_user_text.is_none() && kind == "user" {
                if let Some(msg) = &line.message {
                    if let Some(text) = first_textish(&msg.content) {
                        first_user_text = Some(text);
                    }
                }
            }
        }
    }

    let Some(id) = id.or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_string))
    else {
        return Ok(None);
    };

    let title = ai_title
        .or_else(|| first_user_text.as_deref().map(clean_title))
        .unwrap_or_else(|| "(untitled session)".to_string());

    Ok(Some(SessionSummary {
        id,
        provider: Provider::Claude,
        title,
        project: cwd.unwrap_or_default(),
        started_at,
        updated_at,
        message_count,
        path: path.to_path_buf(),
    }))
}

/// Pull the first piece of human-meaningful text out of a content Value
/// (string, or first text/thinking block in an array).
fn first_textish(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => arr.iter().find_map(|item| {
            let block = serde_json::from_value::<RawBlock>(item.clone()).ok()?;
            block.text.or(block.thinking)
        }),
        _ => None,
    }
}

/// Tidy a string for use as a fallback title (trim + cap at 70 chars).
fn clean_title(s: &str) -> String {
    let trimmed = s.trim();
    trimmed.chars().take(70).collect()
}

/// Fully load a session file into the unified model.
fn load_file(path: &Path) -> Result<Session> {
    let summary = summarize_file(path)?.context("empty session file")?;
    let lines = read_raw_lines(path)?;

    let mut messages = Vec::new();
    for line in &lines {
        let Some(kind) = line.kind.as_deref() else { continue };
        if !matches!(kind, "user" | "assistant") {
            continue;
        }
        let Some(msg) = &line.message else { continue };
        let role = match msg.role.as_deref() {
            Some("assistant") => Role::Assistant,
            Some("user") => Role::User,
            _ => Role::System,
        };
        let blocks = extract_blocks(&msg.content);
        if blocks.is_empty() {
            continue;
        }
        let timestamp = line.timestamp.as_deref().and_then(parse_ts);
        messages.push(Message {
            role,
            timestamp,
            blocks,
        });
    }

    Ok(Session {
        summary,
        messages,
    })
}

/// Normalize a raw content Value (string | array | null) into Blocks.
fn extract_blocks(content: &Value) -> Vec<Block> {
    match content {
        Value::String(s) => {
            if s.trim().is_empty() {
                Vec::new()
            } else {
                vec![Block::Text { text: s.clone() }]
            }
        }
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| {
                let block = serde_json::from_value::<RawBlock>(item.clone()).ok()?;
                raw_block_to_block(block)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn raw_block_to_block(b: RawBlock) -> Option<Block> {
    match b.kind.as_deref()? {
        "text" => b.text.map(|text| Block::Text { text }),
        "thinking" => b.thinking.map(|text| Block::Thinking { text }),
        "tool_use" => {
            let name = b.name.unwrap_or_else(|| "tool".to_string());
            let input = summarize_input(Some(&name), b.input.as_ref());
            Some(Block::ToolUse {
                id: b.id.unwrap_or_default(),
                name,
                input,
            })
        }
        "tool_result" => Some(Block::ToolResult {
            tool_use_id: b.tool_use_id.unwrap_or_default(),
            content: flatten_tool_content(b.content.as_ref()),
            is_error: b.is_error.unwrap_or(false),
        }),
        _ => None, // unknown block type — skip
    }
}

/// Produce a compact, readable summary of a tool_use input.
fn summarize_input(name: Option<&str>, input: Option<&Value>) -> String {
    let Some(value) = input else {
        return String::new();
    };
    // Pull out the most useful field for common tools.
    if let Some(obj) = value.as_object() {
        for key in &["command", "cmd", "pattern", "query", "path", "file_path", "url"] {
            if let Some(v) = obj.get(*key).and_then(Value::as_str) {
                return v.to_string();
            }
        }
        let _ = name;
    }
    // Fall back to compact JSON.
    serde_json::to_string(value).unwrap_or_default()
}

/// Flatten a tool_result's content (string | array of {text} | other) to a string.
fn flatten_tool_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| {
                serde_json::from_value::<RawBlock>(item.clone())
                    .ok()
                    .and_then(|b| b.text)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(content: &str) -> Vec<Block> {
        let v: Value = serde_json::from_str(content).unwrap();
        extract_blocks(&v)
    }

    #[test]
    fn parses_string_content() {
        let blocks = extract("\"hello world\"");
        assert_eq!(blocks, vec![Block::Text { text: "hello world".into() }]);
    }

    #[test]
    fn parses_text_and_thinking_blocks() {
        let blocks = extract(
            r#"[
              {"type":"thinking","thinking":"pondering"},
              {"type":"text","text":"hi there"}
            ]"#,
        );
        assert_eq!(
            blocks,
            vec![
                Block::Thinking { text: "pondering".into() },
                Block::Text { text: "hi there".into() },
            ]
        );
    }

    #[test]
    fn parses_tool_use_block_and_pulls_command() {
        let blocks = extract(
            r#"[{"type":"tool_use","id":"call_1","name":"Bash",
                "input":{"command":"ls -la","description":"list"}}]"#,
        );
        assert_eq!(
            blocks,
            vec![Block::ToolUse {
                id: "call_1".into(),
                name: "Bash".into(),
                input: "ls -la".into(),
            }]
        );
    }

    #[test]
    fn parses_tool_result_string_and_array() {
        let s = extract(r#"[{"type":"tool_result","tool_use_id":"c1","content":"done"}]"#);
        assert_eq!(
            s,
            vec![Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "done".into(),
                is_error: false,
            }]
        );
        let a = extract(
            r#"[{"type":"tool_result","tool_use_id":"c1","is_error":true,
                "content":[{"type":"text","text":"Exit code 1"},{"type":"text","text":"boom"}]}]"#,
        );
        assert_eq!(
            a,
            vec![Block::ToolResult {
                tool_use_id: "c1".into(),
                content: "Exit code 1\nboom".into(),
                is_error: true,
            }]
        );
    }

    #[test]
    fn skips_unknown_block_types() {
        let blocks = extract(
            r#"[{"type":"server_tool_use","something":"else"},
               {"type":"text","text":"keep"}]"#,
        );
        assert_eq!(blocks, vec![Block::Text { text: "keep".into() }]);
    }

    #[test]
    fn load_file_builds_messages() {
        let tmp = std::env::temp_dir().join("naked_claude_test.jsonl");
        let jsonl = r#"{"type":"user","sessionId":"abc","cwd":"/tmp/proj","timestamp":"2026-06-30T10:08:50.723Z","message":{"role":"user","content":"hi"}}
{"type":"assistant","timestamp":"2026-06-30T10:08:51.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello!"}]}}
{"type":"ai-title","aiTitle":"Greeting"}
{"type":"mode","mode":"normal"}
"#;
        std::fs::write(&tmp, jsonl).unwrap();
        let session = load_file(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(session.summary.id, "abc");
        assert_eq!(session.summary.title, "Greeting");
        assert_eq!(session.summary.project, "/tmp/proj");
        assert_eq!(session.summary.message_count, 2);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].role, Role::Assistant);
    }
}
