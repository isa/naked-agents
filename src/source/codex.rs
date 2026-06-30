//! Codex session source — **stub**.
//!
//! Codex stores sessions under `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` as
//! `session_meta` / `response_item` / `event_msg` lines, with titles in
//! `~/.codex/session_index.jsonl`. Implementing this is a follow-up; the trait
//! shape and registry slot are in place so adding it touches only this file.

use anyhow::Result;

use crate::model::{Provider, Session, SessionSummary};

pub struct CodexSource;

impl crate::source::SessionSource for CodexSource {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn discover(&self) -> Result<Vec<SessionSummary>> {
        // Not yet implemented — return empty so the tool works without it.
        Ok(Vec::new())
    }

    fn load(&self, _summary: &SessionSummary) -> Result<Session> {
        anyhow::bail!("codex session loading is not implemented yet")
    }
}
