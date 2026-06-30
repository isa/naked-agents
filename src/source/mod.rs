//! Provider sources. Each provider implements [`SessionSource`] and is
//! registered in [`registry`]; the CLI/TUI/search layers iterate the registry
//! and never name a concrete provider.

use anyhow::Result;

use crate::model::{Provider, Session, SessionSummary};

pub mod claude;
pub mod codex;

/// A source of sessions for one provider.
pub trait SessionSource {
    fn provider(&self) -> Provider;

    /// Discover all sessions (metadata only — cheap, no full parse).
    fn discover(&self) -> Result<Vec<SessionSummary>>;

    /// Load a full session by its summary.
    fn load(&self, summary: &SessionSummary) -> Result<Session>;
}

/// The active provider sources. Claude is implemented; Codex is registered but
/// returns no sessions yet (its `discover` is a stub) — wiring it fully later
/// is just filling in that module.
pub fn registry() -> Vec<Box<dyn SessionSource>> {
    vec![
        Box::new(claude::ClaudeSource::default()),
        Box::new(codex::CodexSource),
    ]
}

/// Discover sessions across all providers, sorted newest-first.
pub fn discover_all() -> Result<Vec<SessionSummary>> {
    let mut all = Vec::new();
    for src in registry() {
        match src.discover() {
            Ok(mut sessions) => all.append(&mut sessions),
            Err(e) => {
                // One provider failing shouldn't kill the whole tool.
                eprintln!("warning: {} discovery failed: {e}", src.provider().as_str());
            }
        }
    }
    sort_newest_first(&mut all);
    Ok(all)
}

/// Load a full session by routing to the owning provider source.
pub fn load_session(summary: &SessionSummary) -> Result<Session> {
    for src in registry() {
        if src.provider() == summary.provider {
            return src.load(summary);
        }
    }
    anyhow::bail!("no source registered for provider {:?}", summary.provider);
}

/// Filter summaries by provider and/or project substring.
pub fn filter(
    mut sessions: Vec<SessionSummary>,
    provider: Option<Provider>,
    project: Option<&str>,
) -> Vec<SessionSummary> {
    sessions.retain(|s| provider.map_or(true, |p| s.provider == p));
    if let Some(proj) = project {
        let proj = proj.to_ascii_lowercase();
        sessions.retain(|s| s.project.to_ascii_lowercase().contains(&proj));
    }
    sessions
}

pub fn sort_newest_first(sessions: &mut [SessionSummary]) {
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
}
