//! Transcript rendering — the single source of truth for how a `Session`
//! becomes text. Produces styled logical lines ([`RenderLine`]) that both the
//! ANSI CLI sink and the ratatui TUI consume.

use chrono::{DateTime, Utc};
use unicode_width::UnicodeWidthStr;

use crate::model::{Block, Message, Role, Session};

/// What to render in a transcript.
#[derive(Debug, Clone, Copy)]
pub struct ShowOptions {
    pub show_thinking: bool,
    pub show_tools: bool,
    /// Max lines shown for a single tool result / tool-use input.
    pub max_tool_lines: usize,
    /// Minimal, unstyled text only (for piping to grep/less).
    pub raw: bool,
}

impl Default for ShowOptions {
    fn default() -> Self {
        Self {
            show_thinking: false,
            show_tools: true,
            max_tool_lines: 12,
            raw: false,
        }
    }
}

/// Styling class for a span of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Title,
    Plain,
    User,
    Assistant,
    System,
    Tool,
    ToolResult,
    ToolError,
    Thinking,
    Dim,
}

#[derive(Debug, Clone)]
pub struct RenderSpan {
    pub text: String,
    pub kind: SpanKind,
}

impl RenderSpan {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SpanKind::Plain,
        }
    }
    fn typed(kind: SpanKind, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

/// A logical line: a sequence of styled spans.
pub type RenderLine = Vec<RenderSpan>;

fn line(spans: Vec<RenderSpan>) -> RenderLine {
    spans
}

/// Render an entire session into logical lines.
pub fn render_session(session: &Session, opts: &ShowOptions) -> Vec<RenderLine> {
    let mut out = Vec::new();
    for msg in &session.messages {
        render_message(msg, opts, &mut out);
    }
    out
}

/// Render a compact header "section" for a session: its title, the full
/// session id, and a one-line metadata summary. Used to anchor the top of a
/// session's details (e.g. the TUI conversation pane), mirroring the `show`
/// command's banner.
pub fn render_header(session: &Session) -> Vec<RenderLine> {
    let started = session
        .summary
        .started_at
        .map(fmt_date)
        .unwrap_or_else(|| "?".into());
    vec![
        line(vec![RenderSpan::typed(
            SpanKind::Title,
            session.summary.title.clone(),
        )]),
        line(vec![
            RenderSpan::typed(SpanKind::Dim, "SESSIONID: "),
            RenderSpan::plain(session.summary.id.clone()),
        ]),
        line(vec![RenderSpan::typed(
            SpanKind::Dim,
            format!(
                "{} · {} messages · {}",
                session.summary.provider.as_str(),
                session.messages.len(),
                started,
            ),
        )]),
        // Blank line separates the header from the transcript below.
        line(vec![]),
    ]
}

fn render_message(msg: &Message, opts: &ShowOptions, out: &mut Vec<RenderLine>) {
    if opts.raw {
        for b in &msg.blocks {
            if let Block::Text { text } = b {
                for l in text.lines() {
                    out.push(line(vec![RenderSpan::plain(l)]));
                }
            }
        }
        return;
    }

    // Skip pure-tool-result messages' header clutter only if tools hidden.
    let visible_blocks: Vec<&Block> = msg
        .blocks
        .iter()
        .filter(|b| block_visible(b, opts))
        .collect();
    if visible_blocks.is_empty() {
        return;
    }

    // Header line.
    let role_kind = match msg.role {
        Role::User => SpanKind::User,
        Role::Assistant => SpanKind::Assistant,
        Role::System => SpanKind::System,
    };
    let label = role_label(msg);
    let mut header = vec![
        RenderSpan::typed(SpanKind::Dim, "─ "),
        RenderSpan::typed(role_kind, label),
    ];
    if let Some(ts) = msg.timestamp {
        header.push(RenderSpan::typed(SpanKind::Dim, format!(" · {}", fmt_clock(ts))));
    }
    header.push(RenderSpan::typed(SpanKind::Dim, " ─"));
    out.push(line(header));

    for b in &visible_blocks {
        render_block(b, opts, out);
    }
    out.push(line(vec![])); // blank separator
}

fn block_visible(b: &Block, opts: &ShowOptions) -> bool {
    match b {
        Block::Text { .. } => true,
        Block::Thinking { .. } => opts.show_thinking,
        Block::ToolUse { .. } | Block::ToolResult { .. } => opts.show_tools,
    }
}

fn render_block(b: &Block, opts: &ShowOptions, out: &mut Vec<RenderLine>) {
    match b {
        Block::Text { text } => {
            for l in text.lines() {
                if l.is_empty() {
                    out.push(line(vec![]));
                } else {
                    // 2-space indent nests the body under its speaker header.
                    out.push(line(vec![RenderSpan::plain(format!("  {l}"))]));
                }
            }
        }
        Block::Thinking { text } => {
            for l in text.lines() {
                out.push(line(vec![
                    RenderSpan::typed(SpanKind::Thinking, format!("  {l}")),
                ]));
            }
        }
        Block::ToolUse { name, input, .. } => {
            let mut spans = vec![
                RenderSpan::typed(SpanKind::Tool, format!("  ▸ {name}")),
            ];
            let input = input.trim();
            if !input.is_empty() {
                let first = input.lines().next().unwrap_or("");
                let mut s: String = first.chars().take(160).collect();
                if input.lines().count() > 1 || first.width() > 160 {
                    s.push('…');
                }
                spans.push(RenderSpan::typed(SpanKind::Dim, format!("  {s}")));
            }
            out.push(line(spans));
        }
        Block::ToolResult {
            content,
            is_error,
            ..
        } => {
            let kind = if *is_error {
                SpanKind::ToolError
            } else {
                SpanKind::ToolResult
            };
            let lines: Vec<&str> = content.lines().collect();
            let cap = opts.max_tool_lines;
            for l in lines.iter().take(cap) {
                out.push(line(vec![RenderSpan::typed(kind, format!("  ↳ {l}"))]));
            }
            if lines.len() > cap {
                out.push(line(vec![RenderSpan::typed(
                    SpanKind::Dim,
                    format!("  ↳ … +{} more lines", lines.len() - cap),
                )]));
            }
            if lines.is_empty() {
                out.push(line(vec![RenderSpan::typed(kind, "  ↳".to_string())]));
            }
        }
    }
}

fn role_label(msg: &Message) -> &'static str {
    if msg.is_only_tool_results() {
        "tool result"
    } else {
        msg.role.label()
    }
}

// ---- ANSI sink (CLI) ----

const RESET: &str = "\x1b[0m";

fn ansi_prefix(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::Title => "\x1b[1m",       // bold
        SpanKind::Plain => "",
        SpanKind::User => "\x1b[36m",       // cyan
        SpanKind::Assistant => "\x1b[34m",  // blue
        SpanKind::System => "\x1b[2;33m",   // dim yellow
        SpanKind::Tool => "\x1b[38;2;255;176;0m", // true-color amber (named yellow renders gray in some terminals)
        SpanKind::ToolResult => "\x1b[2;37m",// dim white/gray
        SpanKind::ToolError => "\x1b[31m",  // red
        SpanKind::Thinking => "\x1b[2;3m",  // dim italic
        SpanKind::Dim => "\x1b[2m",         // dim
    }
}

/// Render logical lines to an ANSI string. `width > 0` enables word-wrap.
pub fn fmt_ansi(lines: &[RenderLine], width: usize, color: bool) -> String {
    let mut buf = String::new();
    for line in lines {
        if width > 0 {
            for wrapped in wrap_line(line, width) {
                emit_line(&wrapped, color, &mut buf);
            }
        } else {
            emit_line(line, color, &mut buf);
        }
    }
    buf
}

fn emit_line(line: &RenderLine, color: bool, buf: &mut String) {
    if color {
        for span in line {
            if span.text.is_empty() {
                continue;
            }
            buf.push_str(ansi_prefix(span.kind));
            buf.push_str(&span.text);
            buf.push_str(RESET);
        }
    } else {
        for span in line {
            buf.push_str(&span.text);
        }
    }
    buf.push('\n');
}

/// Word-wrap a styled line to `width` columns, returning one or more lines.
/// Splits on spaces; style is carried per-word. A leading run of spaces (an
/// indent) is treated as a **block indent**: every wrapped physical line
/// repeats it, not just the first.
pub fn wrap_line(line: &RenderLine, width: usize) -> Vec<RenderLine> {
    if line.is_empty() {
        return vec![line.to_vec()];
    }
    // Capture the line's leading-space indent (block indent, applied to every line).
    let flat: String = line.iter().map(|s| s.text.as_str()).collect();
    let indent: String = flat.chars().take_while(|c| *c == ' ').collect();
    let indent_w = indent.width();
    let indent_kind = line.first().map(|s| s.kind).unwrap_or(SpanKind::Plain);
    let wrap_width = width.saturating_sub(indent_w).max(1);

    // Tokenize into (word, kind), dropping all whitespace (indent is reapplied).
    let mut tokens: Vec<(String, SpanKind)> = Vec::new();
    for span in line {
        for word in span.text.split(' ') {
            if !word.is_empty() {
                tokens.push((word.to_string(), span.kind));
            }
        }
    }

    let mut out = Vec::<RenderLine>::new();
    let mut cur = RenderLine::new();
    let mut cur_width = 0usize;
    for (word, kind) in tokens {
        let w = word.width();
        let needs_space = !cur.is_empty();
        if needs_space && cur_width + 1 + w > wrap_width {
            out.push(std::mem::take(&mut cur));
            cur.push(RenderSpan::typed(kind, word));
            cur_width = w;
        } else {
            if needs_space {
                cur.push(RenderSpan::typed(kind, " "));
                cur_width += 1;
            }
            cur.push(RenderSpan::typed(kind, word));
            cur_width += w;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }

    // Prepend the block indent to every physical line.
    if !indent.is_empty() {
        for pline in out.iter_mut() {
            let mut indented = vec![RenderSpan::typed(indent_kind, indent.clone())];
            indented.append(pline);
            *pline = indented;
        }
    }
    out
}

// ---- time formatting helpers (used by list + show) ----

pub fn fmt_clock(ts: DateTime<Utc>) -> String {
    ts.format("%H:%M").to_string()
}

pub fn fmt_date(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M").to_string()
}

/// Human-friendly relative time, e.g. "5m ago", "3h ago", "2d ago".
pub fn time_ago(ts: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let d = now.signed_duration_since(ts);
    let mins = d.num_minutes();
    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if mins < 60 * 24 {
        format!("{}h ago", mins / 60)
    } else if mins < 60 * 24 * 30 {
        format!("{}d ago", mins / (60 * 24))
    } else {
        fmt_date(ts)
    }
}

/// Short, table-friendly id (first 8 chars).
pub fn short_id(id: &str) -> &str {
    match id.get(..8) {
        Some(s) => s,
        None => id,
    }
}
