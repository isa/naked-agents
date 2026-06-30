//! Interactive TUI: a session-list pane ↔ conversation pane.
//!
//! Built on ratatui + crossterm. Transcript rendering reuses [`crate::format`]
//! (the same `RenderLine` stream the CLI uses), so layout stays consistent.
//!
//! In-conversation search: `/` starts a query, matches highlight live, and
//! `n`/`N` jump forward/backward (vim/less style).

use std::io::{self, IsTerminal, Stdout};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use ratatui::Terminal;

use crate::format::{self, RenderLine, ShowOptions};
use crate::model::{Provider, Session, SessionSummary};
use crate::source;

type Term = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Normal,
    /// Typing a search query.
    Search,
}

/// Which pane receives j/k navigation.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Focus {
    List,
    Conversation,
}

pub struct App {
    /// All sessions (the list pane shows these directly).
    all: Vec<SessionSummary>,
    mode: Mode,
    focus: Focus,
    selected: usize,
    loaded: Option<Session>,
    loaded_id: Option<String>,
    scroll: u16,
    conv_max_scroll: u16,
    conv_height: u16,
    conv_width: usize,
    /// Physical (wrapped) lines of the loaded conversation, cached for search.
    conv_physical: Vec<RenderLine>,
    show_tools: bool,
    show_thinking: bool,
    /// Search buffer while typing in [`Mode::Search`].
    search_input: String,
    /// Committed search query (lowercased), drives n/N + highlight in Normal mode.
    search: Option<String>,
    /// `(line_idx, byte_lo, byte_hi)` of each match within `conv_physical`.
    search_matches: Vec<(usize, usize, usize)>,
    /// Index into `search_matches` for the current n/N target.
    search_pos: Option<usize>,
    now: chrono::DateTime<Utc>,
    quit: bool,
}

impl App {
    fn new(all: Vec<SessionSummary>) -> Self {
        Self {
            all,
            mode: Mode::Normal,
            focus: Focus::List,
            selected: 0,
            loaded: None,
            loaded_id: None,
            scroll: 0,
            conv_max_scroll: 0,
            conv_height: 0,
            conv_width: 80,
            conv_physical: Vec::new(),
            show_tools: true,
            show_thinking: false,
            search_input: String::new(),
            search: None,
            search_matches: Vec::new(),
            search_pos: None,
            now: Utc::now(),
            quit: false,
        }
    }

    /// The query to highlight against right now: the typing buffer in Search
    /// mode, otherwise the committed search.
    fn effective_query(&self) -> Option<String> {
        if self.mode == Mode::Search {
            let q = self.search_input.trim();
            if q.is_empty() {
                None
            } else {
                Some(q.to_ascii_lowercase())
            }
        } else {
            self.search.clone()
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let n = self.all.len();
        if n == 0 {
            return;
        }
        let max = n - 1;
        let next = (self.selected as i32 + delta).clamp(0, max as i32) as usize;
        if next != self.selected {
            self.selected = next;
            self.scroll = 0;
            self.load_selected();
        }
    }

    fn move_to(&mut self, idx: usize) {
        let n = self.all.len();
        if n == 0 {
            return;
        }
        let idx = idx.min(n - 1);
        if idx != self.selected {
            self.selected = idx;
            self.scroll = 0;
            self.load_selected();
        }
    }

    fn load_selected(&mut self) {
        let Some(summary) = self.all.get(self.selected) else {
            self.loaded = None;
            self.loaded_id = None;
            return;
        };
        if self.loaded_id.as_deref() == Some(&summary.id) {
            return;
        }
        match source::load_session(summary) {
            Ok(session) => {
                self.loaded_id = Some(summary.id.clone());
                self.loaded = Some(session);
                self.scroll = 0;
            }
            Err(_) => {
                self.loaded = None;
                self.loaded_id = None;
            }
        }
    }

    fn clamp_scroll(&mut self) {
        if self.scroll > self.conv_max_scroll {
            self.scroll = self.conv_max_scroll;
        }
    }

    // ---- conversation + search bookkeeping ----

    /// Re-render the loaded conversation into physical lines and refresh matches.
    fn prepare_conversation(&mut self) {
        let Some(session) = self.loaded.as_ref() else {
            self.conv_physical.clear();
            self.search_matches.clear();
            return;
        };
        let opts = ShowOptions {
            show_thinking: self.show_thinking,
            show_tools: self.show_tools,
            max_tool_lines: 8,
            raw: false,
        };
        let logical = format::render_session(session, &opts);
        let wrap_w = self.conv_width.max(10);
        let mut physical: Vec<RenderLine> = Vec::with_capacity(logical.len());
        for line in &logical {
            if line.is_empty() {
                physical.push(vec![]);
            } else {
                physical.extend(format::wrap_line(line, wrap_w));
            }
        }
        self.conv_physical = physical;
        self.recompute_matches();
    }

    fn recompute_matches(&mut self) {
        let Some(query) = self.effective_query() else {
            self.search_matches.clear();
            self.search_pos = None;
            return;
        };
        let mut matches = Vec::new();
        for (i, line) in self.conv_physical.iter().enumerate() {
            for (lo, hi) in find_line_matches(line, &query) {
                matches.push((i, lo, hi));
            }
        }
        let n = matches.len();
        match self.search_pos {
            Some(p) if p < n => {}
            _ => self.search_pos = if n > 0 { Some(0) } else { None },
        }
        self.search_matches = matches;
    }

    fn current_match(&self) -> Option<(usize, usize, usize)> {
        self.search_pos
            .and_then(|p| self.search_matches.get(p).copied())
    }

    /// First match at or below the current scroll position (wraps to 0).
    fn first_match_from_scroll(&self) -> usize {
        let top = self.scroll as usize;
        for (i, (line, _, _)) in self.search_matches.iter().enumerate() {
            if *line >= top {
                return i;
            }
        }
        0
    }

    fn jump_to_match(&mut self, idx: usize) {
        let n = self.search_matches.len();
        if idx >= n {
            return;
        }
        self.search_pos = Some(idx);
        let line = self.search_matches[idx].0;
        let target = line.saturating_sub(self.conv_height as usize / 3);
        self.scroll = target.min(self.conv_max_scroll as usize) as u16;
    }

    fn cycle_match(&mut self, forward: bool) {
        let n = self.search_matches.len();
        if n == 0 {
            return;
        }
        let idx = match self.search_pos {
            Some(p) => {
                if forward {
                    (p + 1) % n
                } else {
                    (p + n - 1) % n
                }
            }
            None => 0,
        };
        self.jump_to_match(idx);
    }

    fn clear_search(&mut self) {
        self.search = None;
        self.search_matches.clear();
        self.search_pos = None;
    }

    // ---- input ----

    fn handle_key(&mut self, key: &Event) {
        let Event::Key(k) = key else { return };
        if k.kind != KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::Search => self.handle_search_key(k),
            Mode::Normal => self.handle_normal_key(k),
        }
    }

    fn handle_search_key(&mut self, k: &crossterm::event::KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                self.search_input.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let q = self.search_input.trim().to_ascii_lowercase();
                self.search = if q.is_empty() { None } else { Some(q) };
                self.search_input.clear();
                self.mode = Mode::Normal;
                self.prepare_conversation();
                if !self.search_matches.is_empty() {
                    self.jump_to_match(self.first_match_from_scroll());
                }
            }
            KeyCode::Backspace => {
                self.search_input.pop();
                self.recompute_matches();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_input.clear();
                self.recompute_matches();
            }
            KeyCode::Char(c) => {
                self.search_input.push(c);
                self.recompute_matches();
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, k: &crossterm::event::KeyEvent) {
        // Global keys.
        match k.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return;
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
                return;
            }
            KeyCode::Esc => {
                // Clear search highlight if any, else quit.
                if self.search.is_some() {
                    self.clear_search();
                } else {
                    self.quit = true;
                }
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Conversation,
                    Focus::Conversation => Focus::List,
                };
                return;
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.search_input.clear();
                return;
            }
            KeyCode::Char('n') => {
                self.cycle_match(true);
                return;
            }
            KeyCode::Char('N') => {
                self.cycle_match(false);
                return;
            }
            KeyCode::Char('c') => {
                self.show_tools = !self.show_tools;
                return;
            }
            KeyCode::Char('t') => {
                self.show_thinking = !self.show_thinking;
                return;
            }
            _ => {}
        }

        // Pane-local navigation.
        match self.focus {
            Focus::List => match k.code {
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Char('g') => self.move_to(0),
                KeyCode::Char('G') => self.move_to(self.all.len().saturating_sub(1)),
                KeyCode::Enter => {
                    self.scroll = 0;
                    self.load_selected();
                    self.focus = Focus::Conversation;
                }
                _ => {}
            },
            Focus::Conversation => match k.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.scroll = self.scroll.saturating_add(1).min(self.conv_max_scroll);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.scroll = self.scroll.saturating_sub(1);
                }
                KeyCode::Char('g') => self.scroll = 0,
                KeyCode::Char('G') => self.scroll = self.conv_max_scroll,
                KeyCode::PageDown => {
                    let step = (self.conv_height / 2).max(1);
                    self.scroll = self.scroll.saturating_add(step).min(self.conv_max_scroll);
                }
                KeyCode::PageUp => {
                    let step = (self.conv_height / 2).max(1);
                    self.scroll = self.scroll.saturating_sub(step);
                }
                _ => {}
            },
        }
    }
}

/// Case-insensitive match byte-ranges of `needle` within a line's flat text.
fn find_line_matches(line: &RenderLine, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let flat: String = line.iter().map(|s| s.text.as_str()).collect();
    let lower = flat.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(needle) {
        let lo = from + rel;
        out.push((lo, lo + needle.len()));
        from = lo + needle.len();
        if from >= lower.len() {
            break;
        }
    }
    out
}

pub fn run(provider: Option<Provider>, project: Option<&str>) -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("tui requires an interactive terminal (stdout is not a TTY)");
    }
    let sessions = source::filter(source::discover_all()?, provider, project);
    if sessions.is_empty() {
        anyhow::bail!("no sessions found");
    }

    // Panic-safe terminal teardown.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(sessions);
    app.load_selected();

    let result = run_loop(&mut terminal, &mut app);

    restore_terminal()?;
    result
}

fn run_loop(terminal: &mut Term, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(250))? {
            if let Ok(ev) = event::read() {
                app.handle_key(&ev);
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(f.area());

    // ---- left: session list ----
    let list_title = format!(" Sessions ({}) ", app.all.len());
    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_style(app.focus == Focus::List))
        .title(list_title);

    let now = app.now;
    let items: Vec<ListItem> = app
        .all
        .iter()
        .map(|s| {
            let title = truncate_str(&s.title, chunks[0].width as usize);
            let ago = s
                .updated_at
                .map(|t| format::time_ago(t, now))
                .unwrap_or_else(|| "?".into());
            let meta = format!(
                "{} · {} msgs · {}",
                format::short_id(&s.id),
                s.message_count,
                ago
            );
            ListItem::new(Text::from(vec![
                Line::from(Span::styled(
                    title,
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(meta, Style::default().fg(Color::DarkGray))),
            ]))
        })
        .collect();

    let list = if items.is_empty() {
        List::new(vec![ListItem::new(Line::from(Span::styled(
            "  (no sessions)",
            Style::default().fg(Color::DarkGray),
        )))])
        .block(list_block)
    } else {
        List::new(items)
            .block(list_block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ")
    };

    let mut state = ListState::default();
    if !app.all.is_empty() {
        state.select(Some(app.selected.min(app.all.len() - 1)));
    }
    f.render_stateful_widget(list, chunks[0], &mut state);

    // ---- right: conversation ----
    let conv_block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_style(app.focus == Focus::Conversation))
        .title(format!(
            " {} ",
            app.loaded
                .as_ref()
                .map(|s| s.summary.title.clone())
                .unwrap_or_else(|| "(no session)".to_string())
        ));
    let inner = conv_block.inner(chunks[1]);
    app.conv_width = (inner.width as usize).saturating_sub(1);
    app.conv_height = inner.height;
    f.render_widget(conv_block, chunks[1]);

    app.prepare_conversation();
    app.conv_max_scroll = app
        .conv_physical
        .len()
        .saturating_sub(inner.height as usize) as u16;
    app.clamp_scroll();

    if app.conv_physical.is_empty() {
        f.render_widget(Paragraph::new("select a session"), inner);
    } else {
        let current = app.current_match();
        let physical = &app.conv_physical;
        let matches = &app.search_matches;
        let scroll = app.scroll;
        let lines: Vec<Line> = physical
            .iter()
            .enumerate()
            .map(|(i, pline)| {
                let ms: Vec<(usize, usize)> = matches
                    .iter()
                    .filter_map(|(li, lo, hi)| (*li == i).then_some((*lo, *hi)))
                    .collect();
                let cur_line = current
                    .and_then(|(cli, lo, hi)| (cli == i).then_some((lo, hi)));
                render_conv_line(pline, &ms, cur_line)
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), inner);
    }

    // ---- footer ----
    draw_footer(f, app);
}

fn draw_footer(f: &mut Frame, app: &App) {
    let bottom = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area())[1];

    let mut spans: Vec<Span> = Vec::new();
    match app.mode {
        Mode::Search => {
            spans.push(Span::styled(
                "search: ",
                Style::default().fg(Color::Yellow),
            ));
            spans.push(Span::raw(app.search_input.clone()));
            spans.push(Span::styled("▏", Style::default().fg(Color::Yellow)));
            let n = app.search_matches.len();
            spans.push(Span::styled(
                format!("  {n} match{}  · Enter=jump · Esc=cancel", if n == 1 { "" } else { "es" }),
                Style::default().fg(Color::DarkGray),
            ));
        }
        Mode::Normal => {
            let pane = match app.focus {
                Focus::List => "Sessions: j/k move · Enter open",
                Focus::Conversation => "Conversation: j/k scroll",
            };
            spans.push(Span::styled(
                format!("{pane} · Tab pane · / search · n/N next/prev · c tools · t thinking · q quit"),
                Style::default().fg(Color::DarkGray),
            ));
            if let Some(q) = &app.search {
                spans.push(Span::styled(
                    format!("   · /{q} ({} matches)", app.search_matches.len()),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), bottom);
}

/// Render a physical line, highlighting all `matches` and the `current` one.
fn render_conv_line(
    line: &RenderLine,
    matches: &[(usize, usize)],
    current: Option<(usize, usize)>,
) -> Line<'static> {
    if line.is_empty() {
        return Line::default();
    }
    let pieces = build_pieces(line, matches, current);
    let spans: Vec<Span> = pieces
        .into_iter()
        .map(|(t, s)| Span::styled(t, s))
        .collect();
    Line::from(spans)
}

/// Split a line into (text, style) pieces, applying highlight styles to matches.
fn build_pieces(
    line: &RenderLine,
    matches: &[(usize, usize)],
    current: Option<(usize, usize)>,
) -> Vec<(String, Style)> {
    let mut offset = 0usize;
    let mut pieces: Vec<(String, Style)> = Vec::new();
    for span in line {
        if span.text.is_empty() {
            continue;
        }
        let base = style_for_span(span);
        for ch in span.text.chars() {
            let clen = ch.len_utf8();
            let cstart = offset;
            let cend = offset + clen;
            offset = cend;
            let style = classify(cstart, cend, matches, current, base);
            match pieces.last_mut() {
                Some((t, s)) if *s == style => t.push(ch),
                _ => pieces.push((ch.to_string(), style)),
            }
        }
    }
    pieces
}

fn classify(
    cstart: usize,
    cend: usize,
    matches: &[(usize, usize)],
    current: Option<(usize, usize)>,
    base: Style,
) -> Style {
    if let Some((clo, chi)) = current {
        if cstart < chi && cend > clo {
            return current_style();
        }
    }
    for &(lo, hi) in matches {
        if cstart < hi && cend > lo {
            return match_style();
        }
    }
    base
}

/// Amber highlight for ordinary matches. Uses true-color RGB because some
/// terminals render the 16-color "yellow" slot as gray/muted — RGB bypasses
/// the palette entirely.
fn match_style() -> Style {
    Style::default()
        .fg(Color::Rgb(24, 24, 24))
        .bg(Color::Rgb(255, 176, 0))
        .add_modifier(Modifier::BOLD)
}

/// Brighter, lighter amber + underline for the current (n/N) match, so it's
/// clearly distinct from the medium-amber ordinary matches.
fn current_style() -> Style {
    Style::default()
        .fg(Color::Rgb(24, 16, 0))
        .bg(Color::Rgb(255, 232, 90))
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED)
}

/// Border style for a pane: highlighted when it has focus.
fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn style_for_span(span: &crate::format::RenderSpan) -> Style {
    use crate::format::SpanKind;
    match span.kind {
        SpanKind::Plain => Style::default(),
        SpanKind::User => Style::default().fg(Color::Cyan),
        SpanKind::Assistant => Style::default().fg(Color::Blue),
        SpanKind::System => Style::default().fg(Color::DarkGray),
        SpanKind::Tool => Style::default().fg(Color::Rgb(255, 176, 0)),
        SpanKind::ToolResult => Style::default().fg(Color::DarkGray),
        SpanKind::ToolError => Style::default().fg(Color::Red),
        SpanKind::Thinking => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        SpanKind::Dim => Style::default().fg(Color::DarkGray),
    }
}

/// Truncate a string to fit `max` columns, appending an ellipsis if cut.
fn truncate_str(s: &str, max: usize) -> String {
    let max = if max >= 4 { max - 2 } else { 1 };
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            out.push('…');
            break;
        }
        out.push(c);
        width += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::SpanKind;
    use crate::model::{Block, Message, Role};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn buffer_string(buf: &ratatui::buffer::Buffer) -> String {
        let width = buf.area().width as usize;
        let mut s = String::new();
        for (i, cell) in buf.content().iter().enumerate() {
            s.push_str(cell.symbol());
            if (i + 1) % width == 0 {
                s.push('\n');
            }
        }
        s
    }

    fn summary(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            provider: Provider::Claude,
            title: title.into(),
            project: "/tmp".into(),
            started_at: None,
            updated_at: None,
            message_count: 0,
            path: PathBuf::from("/tmp/x.jsonl"),
        }
    }

    fn session_with(id: &str, msgs: Vec<&str>) -> Session {
        let summary = summary(id, "Test");
        let messages = msgs
            .into_iter()
            .map(|t| Message {
                role: Role::User,
                timestamp: None,
                blocks: vec![Block::Text { text: t.into() }],
            })
            .collect();
        Session { summary, messages }
    }

    #[test]
    fn renders_list_and_conversation() {
        let session = session_with("abc12345-xxxx", vec!["hello world", "hi there"]);
        let mut app = App::new(vec![session.summary.clone()]);
        app.loaded = Some(session);
        app.loaded_id = Some("abc12345-xxxx".into());

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let s = buffer_string(terminal.backend().buffer());
        assert!(s.contains("hello world"), "user text missing:\n{s}");
        assert!(s.contains("hi there"), "assistant text missing:\n{s}");
        assert!(s.contains("search"), "footer hints missing:\n{s}");
    }

    #[test]
    fn classify_applies_amber_for_matches() {
        let base = Style::default().fg(Color::Cyan);
        // char range inside an ordinary match → amber
        assert_eq!(classify(2, 3, &[(0, 5)], None, base), match_style());
        // inside the current match → brighter amber
        assert_eq!(classify(2, 3, &[(0, 5)], Some((0, 5)), base), current_style());
        // outside any match → unchanged base style
        assert_eq!(classify(10, 11, &[(0, 5)], None, base), base);
    }

    #[test]
    fn search_finds_and_records_matches() {
        let session = session_with(
            "abc12345",
            vec!["find the hello word here", "nothing relevant", "hello again"],
        );
        let mut app = App::new(vec![session.summary.clone()]);
        app.loaded = Some(session);
        app.loaded_id = Some("abc12345".into());
        app.conv_width = 80;
        app.search = Some("hello".into());
        app.prepare_conversation();

        assert_eq!(app.search_matches.len(), 2, "should find two 'hello' matches");
        // n/N cycling lands on both.
        let first = app.search_pos;
        app.cycle_match(true);
        assert_ne!(app.search_pos, first);
        app.cycle_match(false);
        assert_eq!(app.search_pos, first);
    }

    #[test]
    fn search_highlights_render_without_panicking() {
        let session = session_with("abc12345", vec!["find the hello word here"]);
        let mut app = App::new(vec![session.summary.clone()]);
        app.loaded = Some(session);
        app.loaded_id = Some("abc12345".into());
        app.conv_width = 80;
        app.search = Some("hello".into());
        app.prepare_conversation();

        let line_idx = app.search_matches[0].0;
        let cur = app.current_match();
        let ms: Vec<(usize, usize)> = app
            .search_matches
            .iter()
            .filter_map(|(i, lo, hi)| (*i == line_idx).then_some((*lo, *hi)))
            .collect();
        let cur_line = cur.and_then(|(i, lo, hi)| (i == line_idx).then_some((lo, hi)));
        let line = render_conv_line(&app.conv_physical[line_idx], &ms, cur_line);
        assert!(!line.spans.is_empty(), "highlighted line should have spans");
    }

    #[test]
    fn tab_switches_focus_and_jk_targets_focused_pane() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let mk = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));

        let mut app = App::new(vec![summary("aaa11111", "First"), summary("bbb22222", "Second")]);
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.selected, 0);

        app.handle_key(&mk(KeyCode::Char('j')));
        assert_eq!(app.selected, 1);
        app.handle_key(&mk(KeyCode::Char('k')));
        assert_eq!(app.selected, 0);

        app.handle_key(&mk(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Conversation);

        app.conv_max_scroll = 50;
        app.handle_key(&mk(KeyCode::Char('j')));
        assert_eq!(app.scroll, 1);
        app.handle_key(&mk(KeyCode::Char('k')));
        assert_eq!(app.scroll, 0);

        app.handle_key(&mk(KeyCode::Tab));
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn block_indent_applies_to_wrapped_lines() {
        // A 2-space-indented line wider than `width` wraps, and every wrapped
        // physical line should keep the 2-space indent.
        let line: RenderLine = vec![crate::format::RenderSpan {
            text: "  one two three four five six seven eight nine ten".to_string(),
            kind: SpanKind::Plain,
        }];
        let wrapped = format::wrap_line(&line, 20);
        for pline in &wrapped {
            let flat: String = pline.iter().map(|s| s.text.as_str()).collect();
            assert!(
                flat.starts_with("  "),
                "wrapped line lost its block indent: {flat:?}"
            );
        }
    }
}
