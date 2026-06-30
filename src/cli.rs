//! Command-line interface (`list`, `show`).

use std::io::{IsTerminal, Write};

use anyhow::{anyhow, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};

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
        Cmd::List(args) => cmd_list(args),
        Cmd::Show(args) => cmd_show(args, color),
        Cmd::Search(args) => cmd_search(args, color),
        Cmd::Latest(args) => cmd_latest(args, color),
        Cmd::Tui(args) => cmd_tui(args),
    }
}

fn cmd_list(args: ListArgs) -> Result<()> {
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
    print_session_table(&sessions[..shown]);

    if total > shown {
        eprintln!("\n(showing {shown} of {total}; use --limit to see more)");
    } else {
        eprintln!("\n{total} session(s)");
    }
    Ok(())
}

fn print_session_table(sessions: &[SessionSummary]) {
    use comfy_table::{ContentArrangement, Table};

    let now = Utc::now();
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Disabled);
    table.set_header(vec!["ID", "PROVIDER", "MSGS", "UPDATED", "TITLE", "PROJECT"]);

    for s in sessions {
        let updated = s
            .updated_at
            .map(|t| format::time_ago(t, now))
            .unwrap_or_else(|| "?".into());
        let title = truncate(s.title.as_str(), 42);
        let project = truncate(
            s.project.rsplit('/').next().unwrap_or(&s.project),
            20,
        );
        table.add_row(vec![
            format::short_id(&s.id).to_string(),
            s.provider.as_str().to_string(),
            s.message_count.to_string(),
            updated,
            title,
            project.to_string(),
        ]);
    }

    let stdout = std::io::stdout();
    let _ = writeln!(stdout.lock(), "{table}");
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
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
            "{title}\n{provider} · {id} · {msgs} messages · {started}",
            title = session.summary.title,
            provider = session.summary.provider.as_str(),
            id = session.summary.id,
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
