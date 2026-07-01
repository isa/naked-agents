//! Command-line interface (`list`, `show`).

use std::io::{IsTerminal, Write};

use anyhow::{anyhow, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};

use crate::format::{self, ShowOptions};
use crate::model::{Provider, Session, SessionSummary};
use crate::search;
use crate::source;

#[derive(Parser)]
#[command(
    name = "naked",
    version,
    about = "View, browse, and search Claude Code & Codex sessions",
    long_about = None
)]
pub struct Cli {
    /// When to use color. `always` keeps color even when piped (head/tail/grep).
    #[arg(long, value_enum, global = true, default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List all known sessions.
    List(ListArgs),
    /// Show the full transcript of a session.
    Show(ShowArgs),
    /// Search session contents for a query.
    Search(SearchArgs),
    /// Show the most recently updated session.
    Latest(LatestArgs),
    /// Launch the interactive browser.
    Tui(TuiArgs),
}

#[derive(clap::Args)]
pub struct ListArgs {
    /// Filter by provider.
    #[arg(long, value_enum)]
    pub provider: Option<ProviderFilter>,
    /// Filter by project (substring match on the working directory).
    #[arg(long)]
    pub project: Option<String>,
    /// Max number of sessions to print.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Emit JSON (unified model).
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct ShowArgs {
    /// Session id, or a unique prefix / substring of it.
    pub id: String,
    /// Hide tool calls and results.
    #[arg(long)]
    pub no_tools: bool,
    /// Hide model thinking.
    #[arg(long)]
    pub no_thinking: bool,
    /// Show model thinking (hidden by default).
    #[arg(long)]
    pub thinking: bool,
    /// Plain text only, no styling or metadata (pipe-friendly).
    #[arg(long)]
    pub raw: bool,
    /// Emit JSON (unified model).
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct SearchArgs {
    /// Text to search for (case-insensitive, matches anywhere).
    pub query: String,
    /// Filter by provider.
    #[arg(long, value_enum)]
    pub provider: Option<ProviderFilter>,
    /// Filter by project (substring match on the working directory).
    #[arg(long)]
    pub project: Option<String>,
    /// Max number of hits to print.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct LatestArgs {
    /// Hide tool calls and results.
    #[arg(long)]
    pub no_tools: bool,
    /// Show model thinking.
    #[arg(long)]
    pub thinking: bool,
    /// Plain text only (pipe-friendly).
    #[arg(long)]
    pub raw: bool,
}

#[derive(clap::Args)]
pub struct TuiArgs {
    /// Filter by provider.
    #[arg(long, value_enum)]
    pub provider: Option<ProviderFilter>,
    /// Filter by project (substring match on the working directory).
    #[arg(long)]
    pub project: Option<String>,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum ProviderFilter {
    Claude,
    Codex,
}

impl ProviderFilter {
    fn to_provider(self) -> Provider {
        match self {
            ProviderFilter::Claude => Provider::Claude,
            ProviderFilter::Codex => Provider::Codex,
        }
    }
}

/// When to emit ANSI color.
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum ColorWhen {
    Always,
    Auto,
    Never,
}

impl ColorWhen {
    fn active(self) -> bool {
        match self {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => std::io::stdout().is_terminal(),
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let color = cli.color.active();
    match cli.command {
        Cmd::List(args) => cmd_list(args, color),
        Cmd::Show(args) => cmd_show(args, color),
        Cmd::Search(args) => cmd_search(args, color),
        Cmd::Latest(args) => cmd_latest(args, color),
        Cmd::Tui(args) => cmd_tui(args),
    }
}

fn cmd_list(args: ListArgs, color: bool) -> Result<()> {
    let provider = args.provider.map(|p| p.to_provider());
    let mut sessions = source::discover_all()?;
    sessions = source::filter(sessions, provider, args.project.as_deref());

    if args.json {
        let stdout = std::io::stdout();
        serde_json::to_writer_pretty(stdout.lock(), &sessions)?;
        println!();
        return Ok(());
    }

    let total = sessions.len();
    let shown = sessions.len().min(args.limit);
    print_session_table(&sessions[..shown], color);

    if total > shown {
        eprintln!("\n(showing {shown} of {total}; use --limit to see more)");
    } else {
        eprintln!("\n{total} session(s)");
    }
    Ok(())
}

fn print_session_table(sessions: &[SessionSummary], color: bool) {
    let now = Utc::now();

    // Materialize the per-row strings once so we can measure the fixed columns
    // and size the flexible TITLE / PROJECT columns to the terminal width.
    struct Row {
        id: String,
        provider: &'static str,
        msgs: String,
        updated: String,
        title: String,
        project: String,
    }
    let rows: Vec<Row> = sessions
        .iter()
        .map(|s| {
            let updated = s
                .updated_at
                .map(|t| format::time_ago(t, now))
                .unwrap_or_else(|| "?".into());
            let project = s
                .project
                .rsplit('/')
                .next()
                .filter(|p| !p.is_empty())
                .unwrap_or(&s.project)
                .to_string();
            Row {
                id: format::short_id(&s.id).to_string(),
                provider: s.provider.as_str(),
                msgs: s.message_count.to_string(),
                updated,
                title: s.title.clone(),
                project,
            }
        })
        .collect();

    // Fixed columns size to their content; TITLE and PROJECT share whatever
    // width remains so the table always fits the terminal (capped so a very
    // wide terminal doesn't sprawl). Overhead = UTF8_FULL's vertical bars
    // (cols + 1) + 1-space padding either side of each cell (2 * cols), plus a
    // small safety margin for comfy-table's internal spacing.
    let cols = 6usize;
    let fixed = [
        col_width("ID", rows.iter().map(|r| r.id.as_str())),
        col_width("PROVIDER", rows.iter().map(|r| r.provider)),
        col_width("MSGS", rows.iter().map(|r| r.msgs.as_str())),
        col_width("UPDATED", rows.iter().map(|r| r.updated.as_str())),
    ]
    .into_iter()
    .sum::<usize>();
    let overhead = cols + 1 + 2 * cols + 2;
    let avail = term_width().saturating_sub(overhead + fixed);
    // PROJECT gets up to a quarter of the remaining width (at least its header
    // width of 7); TITLE takes the rest. No forced minimums on TITLE, so on a
    // narrow terminal it shrinks to fit instead of overflowing the screen.
    let project_max = (avail / 4).clamp(7, 24);
    let title_max = avail.saturating_sub(project_max).min(70);

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Disabled);
    // comfy-table gates styling on its own tty detection. Take explicit control
    // so `--color always` styles even when piped, and `--color never` stays
    // plain even on an interactive terminal.
    if color {
        table.enforce_styling();
    } else {
        table.force_no_tty();
    }
    table.set_header(vec![
        cell("ID", color, None, true),
        cell("PROVIDER", color, None, true),
        cell("MSGS", color, None, true),
        cell("UPDATED", color, None, true),
        cell("TITLE", color, None, true),
        cell("PROJECT", color, None, true),
    ]);

    for r in &rows {
        let prov_color = match r.provider {
            "claude" => Color::Magenta,
            "codex" => Color::Cyan,
            _ => Color::White,
        };
        table.add_row(vec![
            cell(&r.id, color, Some(Color::DarkCyan), false),
            cell(r.provider, color, Some(prov_color), false),
            cell(&r.msgs, color, None, false),
            cell(&r.updated, color, Some(Color::DarkGrey), false),
            cell(&truncate(&r.title, title_max), color, None, false),
            cell(&truncate(&r.project, project_max), color, Some(Color::DarkGrey), false),
        ]);
    }

    let stdout = std::io::stdout();
    let _ = writeln!(stdout.lock(), "{table}");
}

/// Build a cell, applying color/bold only when `color` is on (so `--color never`
/// and plain pipes stay unstyled).
fn cell(text: &str, color: bool, fg: Option<Color>, bold: bool) -> Cell {
    let mut c = Cell::new(text);
    if color {
        if let Some(fg) = fg {
            c = c.fg(fg);
        }
        if bold {
            c = c.add_attribute(Attribute::Bold);
        }
    }
    c
}

/// Max display width of a column's header and its values.
fn col_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> usize {
    use unicode_width::UnicodeWidthStr;
    let mut w = header.width();
    for v in values {
        w = w.max(v.width());
    }
    w
}

/// Trim and truncate to fit `max` display columns, appending an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let s = s.trim();
    if s.width() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1); // reserve one column for the ellipsis
    let mut out: String = s
        .chars()
        .scan(budget, |left, ch| {
            let w = ch.width().unwrap_or(0);
            if *left >= w {
                *left -= w;
                Some(ch)
            } else {
                None
            }
        })
        .collect();
    out.push('…');
    out
}

fn cmd_show(args: ShowArgs, color: bool) -> Result<()> {
    let sessions = source::discover_all()?;
    let summary = resolve_session(&sessions, &args.id)?;

    if args.json {
        let session = source::load_session(&summary)?;
        let stdout = std::io::stdout();
        serde_json::to_writer_pretty(stdout.lock(), &session)?;
        println!();
        return Ok(());
    }

    let session = source::load_session(&summary)?;
    print_session(&session, &show_options(&args), color)?;
    Ok(())
}

fn show_options(args: &ShowArgs) -> ShowOptions {
    ShowOptions {
        show_thinking: args.thinking && !args.no_thinking,
        show_tools: !args.no_tools,
        max_tool_lines: 12,
        raw: args.raw,
    }
}

fn cmd_search(args: SearchArgs, color: bool) -> Result<()> {
    let provider = args.provider.map(|p| p.to_provider());
    let sessions = source::filter(
        source::discover_all()?,
        provider,
        args.project.as_deref(),
    );
    let hits = search::collect_hits(&sessions, &args.query)?;

    if args.json {
        let stdout = std::io::stdout();
        serde_json::to_writer_pretty(stdout.lock(), &hits)?;
        println!();
        return Ok(());
    }

    let shown: Vec<search::Hit> = hits.iter().take(args.limit).cloned().collect();
    let out = search::fmt_hits(&shown, color);
    let stdout = std::io::stdout();
    let _ = stdout.lock().write_all(out.as_bytes());

    let total = hits.len();
    eprintln!("\n{total} hit(s){}", if total > shown.len() { format!(" (showing {})", shown.len()) } else { String::new() });
    Ok(())
}

fn cmd_latest(args: LatestArgs, color: bool) -> Result<()> {
    let sessions = source::discover_all()?;
    let summary = sessions
        .first()
        .ok_or_else(|| anyhow!("no sessions found"))?;
    let session = source::load_session(summary)?;
    let opts = ShowOptions {
        show_thinking: args.thinking,
        show_tools: !args.no_tools,
        max_tool_lines: 12,
        raw: args.raw,
    };
    print_session(&session, &opts, color)?;
    Ok(())
}

fn cmd_tui(args: TuiArgs) -> Result<()> {
    let provider = args.provider.map(|p| p.to_provider());
    crate::tui::run(provider, args.project.as_deref())
}

fn print_session(session: &Session, opts: &ShowOptions, color: bool) -> Result<()> {
    if !opts.raw {
        // Title banner.
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        writeln!(
            h,
            "SESSIONID: {id}\n{title}\n{provider} · {msgs} messages · {started}",
            id = session.summary.id,
            title = session.summary.title,
            provider = session.summary.provider.as_str(),
            msgs = session.messages.len(),
            started = session
                .summary
                .started_at
                .map(|t| format::fmt_date(t))
                .unwrap_or_else(|| "?".into())
        )?;
        writeln!(h, "{}", "─".repeat(48))?;
    }

    let lines = format::render_session(session, opts);
    let is_tty = std::io::stdout().is_terminal();
    let color = color && !opts.raw;
    // Wrap to terminal width when going to a tty OR when color is forced
    // (--color always, e.g. piped to less/head but still pretty).
    let width = if !opts.raw && (is_tty || color) { term_width() } else { 0 };
    let out = format::fmt_ansi(&lines, width, color);
    let stdout = std::io::stdout();
    let _ = stdout.lock().write_all(out.as_bytes());
    Ok(())
}

/// Resolve a user-supplied id token: exact match first, then unique prefix,
/// then error on ambiguity.
pub(crate) fn resolve_session(sessions: &[SessionSummary], token: &str) -> Result<SessionSummary> {
    let token = token.trim();
    let lower = token.to_ascii_lowercase();

    if let Some(exact) = sessions.iter().find(|s| s.id == token) {
        return Ok(exact.clone());
    }
    let prefix_matches: Vec<&SessionSummary> = sessions
        .iter()
        .filter(|s| s.id.to_ascii_lowercase().starts_with(&lower))
        .collect();
    match prefix_matches.as_slice() {
        [] => {
            // Try a substring-of-title fuzzy fallback.
            let title_hits: Vec<&SessionSummary> = sessions
                .iter()
                .filter(|s| s.title.to_ascii_lowercase().contains(&lower))
                .collect();
            match title_hits.as_slice() {
                [one] => Ok((*one).clone()),
                [] => Err(anyhow!("no session matching '{token}'")),
                _ => Err(anyhow!(
                    "multiple sessions match '{}': {}",
                    token,
                    title_hits
                        .iter()
                        .map(|s| format::short_id(&s.id).to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        [one] => Ok((*one).clone()),
        many => Err(anyhow!(
            "'{}' matches {} sessions; give more characters. Matches: {}",
            token,
            many.len(),
            many.iter()
                .map(|s| format::short_id(&s.id).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn term_width() -> usize {
    // Honor an explicit COLUMNS override (also lets scripts/screenshots pin a width).
    if let Ok(s) = std::env::var("COLUMNS") {
        if let Ok(c) = s.trim().parse::<usize>() {
            if c > 0 {
                return c.max(20);
            }
        }
    }
    let cols = crossterm::terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    cols.max(20)
}
