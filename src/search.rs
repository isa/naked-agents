//! Full-text search across sessions.

use anyhow::Result;
use serde::Serialize;

use crate::model::{Provider, Role, SessionSummary};
use crate::source;

/// One search hit: a session + the matching line/role + a contextual snippet.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub session_id: String,
    pub title: String,
    pub provider: Provider,
    pub role: Role,
    pub snippet: Snippet,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub text: String,
    /// Byte offsets of the match within [`Snippet::text`].
    pub lo: usize,
    pub hi: usize,
}

/// Absolute cap so a runaway query can't produce millions of hits.
const MAX_HITS: usize = 2000;

/// Scan every session's text for case-insensitive occurrences of `query`.
pub fn collect_hits(sessions: &[SessionSummary], query: &str) -> Result<Vec<Hit>> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    let mut hits = Vec::new();
    'outer: for summary in sessions {
        let session = match source::load_session(summary) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for msg in &session.messages {
            let text = msg.full_text();
            let lower = text.to_ascii_lowercase();
            let mut from = 0usize;
            while let Some(rel) = lower[from..].find(&needle) {
                let lo = from + rel;
                let hi = lo + needle.len();
                hits.push(Hit {
                    session_id: summary.id.clone(),
                    title: summary.title.clone(),
                    provider: summary.provider,
                    role: msg.role,
                    snippet: make_snippet(&text, lo, hi),
                });
                from = hi;
                if from >= lower.len() {
                    break;
                }
                if hits.len() >= MAX_HITS {
                    break 'outer;
                }
            }
        }
    }
    Ok(hits)
}

/// Build a ~one-line snippet around a match (byte offsets `lo..hi` in `text`).
fn make_snippet(text: &str, lo: usize, hi: usize) -> Snippet {
    const CTX: usize = 60;
    let start = text.floor_char_boundary(lo.saturating_sub(CTX));
    let end = text.ceil_char_boundary((hi + CTX).min(text.len()));

    let leading = start > 0;
    let trailing = end < text.len();

    let mut snippet = String::new();
    if leading {
        snippet.push('…');
    }
    let body_off = if leading { 1 } else { 0 };

    // Collapse newlines to single spaces (1:1, preserves byte offsets).
    for ch in text[start..end].chars() {
        snippet.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
    }
    if trailing {
        snippet.push('…');
    }

    Snippet {
        lo: body_off + (lo - start),
        hi: body_off + (lo - start) + (hi - lo),
        text: snippet,
    }
}

/// Render hits to text, grouping by session. `color` enables reverse-video on
/// the matched substring.
pub fn fmt_hits(hits: &[Hit], color: bool) -> String {
    const REV: &str = "\x1b[7m";
    const RESET: &str = "\x1b[0m";
    const DIM: &str = "\x1b[2m";
    const CYAN: &str = "\x1b[36m";

    let mut out = String::new();
    let mut current_session: Option<&str> = None;

    for hit in hits {
        if current_session != Some(hit.session_id.as_str()) {
            current_session = Some(hit.session_id.as_str());
            if !out.is_empty() {
                out.push('\n');
            }
            let id = crate::format::short_id(&hit.session_id);
            if color {
                out.push_str(CYAN);
                out.push_str(id);
                out.push_str(RESET);
                out.push_str(DIM);
                out.push_str(" · ");
                out.push_str(&hit.title);
                out.push_str(RESET);
            } else {
                out.push_str(id);
                out.push_str(" · ");
                out.push_str(&hit.title);
            }
            out.push('\n');
        }

        let role = format!("{:>9}", hit.role.label());
        if color {
            out.push_str(DIM);
            out.push_str(&role);
            out.push_str(RESET);
            out.push_str("  ");
            out.push_str(&hit.snippet.text[..hit.snippet.lo]);
            out.push_str(REV);
            out.push_str(&hit.snippet.text[hit.snippet.lo..hit.snippet.hi]);
            out.push_str(RESET);
            out.push_str(&hit.snippet.text[hit.snippet.hi..]);
        } else {
            out.push_str(&role);
            out.push_str("  ");
            // Mark the match with guillemets so it's findable without color.
            out.push_str(&hit.snippet.text[..hit.snippet.lo]);
            out.push('›');
            out.push_str(&hit.snippet.text[hit.snippet.lo..hit.snippet.hi]);
            out.push('‹');
            out.push_str(&hit.snippet.text[hit.snippet.hi..]);
        }
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_offsets_are_within_bounds_and_mark_match() {
        let text = "The quick brown fox jumps over the lazy dog";
        let s = make_snippet(text, 10, 15); // "brown"
        let matched = &s.text[s.lo..s.hi];
        assert_eq!(matched, "brown");
    }

    #[test]
    fn snippet_handles_multibyte_and_newlines() {
        let text = "line one\ncafé résumé match here\nmore";
        let idx = text.find("match").unwrap();
        let s = make_snippet(text, idx, idx + 5);
        assert_eq!(&s.text[s.lo..s.hi], "match");
        assert!(!s.text.contains('\n')); // newlines collapsed
    }
}
