//! Provider-agnostic session model.
//!
//! Every provider (Claude, Codex, ...) maps its on-disk format into these types.
//! The CLI, search, and TUI layers never branch on provider — they consume this
//! model exclusively.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Which agent produced the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// Speaker of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
}

/// A single content block within a message. Mirrors the union of Claude's and
/// Codex's block shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    /// Plain text content.
    Text { text: String },
    /// Model reasoning / chain-of-thought.
    Thinking { text: String },
    /// The model requested a tool be run.
    ToolUse {
        id: String,
        name: String,
        /// Renderable summary of the tool input (compact JSON or a pulled field).
        input: String,
    },
    /// The result of a tool call, returned to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

impl Block {
    /// All text in this block (used for full-text search).
    pub fn full_text(&self) -> String {
        match self {
            Block::Text { text } => text.clone(),
            Block::Thinking { text } => text.clone(),
            Block::ToolUse { name, input, .. } => format!("{name} {input}"),
            Block::ToolResult { content, .. } => content.clone(),
        }
    }
}

/// One conversational turn.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: Role,
    pub timestamp: Option<DateTime<Utc>>,
    pub blocks: Vec<Block>,
}

impl Message {
    /// Convenience: does this message contain only tool results?
    pub fn is_only_tool_results(&self) -> bool {
        !self.blocks.is_empty() && self.blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }))
    }

    /// Concatenated searchable text of all blocks.
    pub fn full_text(&self) -> String {
        self.blocks
            .iter()
            .map(Block::full_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Cheap metadata about a session — enough to list/sort without parsing the
/// full conversation.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub provider: Provider,
    pub title: String,
    /// Working directory the session ran in.
    pub project: String,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    /// Absolute path to the on-disk session file.
    pub path: PathBuf,
}

/// A fully-loaded session.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub messages: Vec<Message>,
}
