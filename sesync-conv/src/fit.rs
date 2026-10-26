//! Context fit: a copy written for a message-based harness must fit that harness's window.
//!
//! The live window (`Journal::messages`: the active branch cut at the last compaction) can still
//! be far larger than the window of the harness that resumes it. An omp session compacted to
//! ~30k tokens was once exported to OpenCode as 3.4M. The fit keeps the compaction summary and
//! the newest turns that fit the budget, and leaves out the older turns.
//!
//! Budget: `window x share - overhead` tokens. Defaults: share 0.75, overhead 16384, both set by
//! `--context-share` / `--context-overhead`; the window by `--context-window`, else 128000.
//!
//! Estimate: `ceil(bytes / 3)` for every string, 8 per message, and per image the tokens of its
//! size (`attach::estimate`), read from the image header: the larger of Claude's 28 px patch rule
//! and OpenAI's 512 px tile rule, 1600 when the header is unreadable. An image the target would
//! refuse costs its note instead (`attach::judge`). Deliberately high: English prose runs near 4
//! bytes per token and code or JSON nearer 3, so the target's real count sits under the estimate.
//! No tokenizer is loaded.
//!
//! Cuts: only at user messages, and only where every tool result after the cut answers a call
//! after it, so a call and its result always travel together. When the summary and the newest
//! turn alone do not fit, the fit refuses and the copy is not written.
//!
//! Only the message-based writers fit (Claude, OpenCode, Antigravity). omp and Pi receive the
//! whole journal and apply their own compaction (see `main.rs`, `to-pi` / `to-omp`).

use crate::attach::{self, Limits};
use crate::journal::{iso_from_ms, Journal, Msg};
use serde_json::Value;
use std::collections::HashMap;

/// Window assumed when the caller gives none: the smallest value the sesync table uses.
pub const DEFAULT_WINDOW: usize = 128_000;
pub const DEFAULT_SHARE: f64 = 0.75;
pub const DEFAULT_OVERHEAD: usize = 16_384;
/// Framing per message: role, separators, the record envelope the target adds.
const TOKENS_PER_MESSAGE: usize = 8;

/// Estimated tokens for `n` bytes of text: `ceil(n / 3)`.
pub fn tokens_for_bytes(n: usize) -> usize {
    n.div_ceil(3)
}

/// The window a copy may fill, from the flags `--context-window`, `--context-share`,
/// `--context-overhead`. Each flag is optional; a value that is not usable is an error.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub window: usize,
    pub share: f64,
    pub overhead: usize,
}

impl Budget {
    pub fn from_args(args: &[String]) -> Result<Budget, String> {
        let get = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let window = match get("--context-window") {
            Some(v) => v.parse::<usize>().ok().filter(|n| *n > 0)
                .ok_or_else(|| format!("--context-window must be a positive token count, not {v:?}"))?,
            None => DEFAULT_WINDOW,
        };
        let share = match get("--context-share") {
            Some(v) => v.parse::<f64>().ok().filter(|s| *s > 0.0 && *s <= 1.0)
                .ok_or_else(|| format!("--context-share must be above 0 and at most 1, not {v:?}"))?,
            None => DEFAULT_SHARE,
        };
        let overhead = match get("--context-overhead") {
            Some(v) => v.parse::<usize>().map_err(|_| format!("--context-overhead must be a token count, not {v:?}"))?,
            None => DEFAULT_OVERHEAD,
        };
        let b = Budget { window, share, overhead };
        if b.limit() == 0 {
            return Err(format!("the budget leaves no room: {} tokens", b.describe()));
        }
        Ok(b)
    }

    /// The estimated tokens a copy may fill.
    pub fn limit(&self) -> usize {
        ((self.window as f64 * self.share) as usize).saturating_sub(self.overhead)
    }

    fn describe(&self) -> String {
        format!("window {} x share {} - overhead {} = {}", self.window, self.share, self.overhead, self.limit())
    }
}

/// What a fit measured, for the converter's log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub turns: usize,
    pub turns_kept: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// None when the window was not fitted.
    pub limit: Option<usize>,
}

impl Report {
    pub fn line(&self, target: &str) -> String {
        let limit = match self.limit {
            Some(l) => format!("limit {l}"),
            None => "not fitted".to_string(),
        };
        format!(
            "fit: {target}: kept {} of {} user turn(s), ~{} of ~{} estimated tokens ({limit})",
            self.turns_kept, self.turns, self.tokens_after, self.tokens_before
        )
    }
}

/// A journal's live window as a message-based writer exports it: the note that stands in for
/// what is not carried (the compaction summary, the turns a fit left out) and the messages after
/// it, oldest first.
pub struct Window<'a> {
    summary: Option<(String, String)>,
    cut: bool,
    pub msgs: Vec<Msg<'a>>,
    pub report: Report,
}

impl Window<'_> {
    /// The note the target reads first, with its ISO time: None when nothing was left out.
    pub fn note(&self) -> Option<(String, String)> {
        if self.summary.is_none() && !self.cut {
            return None;
        }
        let summary = self.summary.as_ref().map(|(t, _)| t.as_str());
        let ts = match &self.summary {
            Some((_, ts)) if !ts.is_empty() => ts.clone(),
            // Just before the first kept message: the note is older than every turn it introduces.
            _ => iso_from_ms(self.msgs.first().map_or(0, |m| m.timestamp_ms) - 1),
        };
        Some((note_text(summary, self.cut), ts))
    }
}

/// The note text. The "Earlier turns of this conversation were" opening is what sesync recognises
/// as its own note, so it is never read as something a person typed.
fn note_text(summary: Option<&str>, cut: bool) -> String {
    match (summary, cut) {
        (Some(s), false) => format!("Earlier turns of this conversation were compacted. Their summary:\n\n{s}"),
        (Some(s), true) => format!(
            "Earlier turns of this conversation were compacted. Their summary:\n\n{s}\n\n\
             More turns after that summary were left out to fit the context window."
        ),
        (None, true) => "Earlier turns of this conversation were left out to fit the context window.".to_string(),
        (None, false) => String::new(),
    }
}

/// Tokens the note costs in the copy (0 when there is no note).
fn note_tokens(summary: Option<&str>, cut: bool) -> usize {
    if summary.is_none() && !cut {
        return 0;
    }
    TOKENS_PER_MESSAGE + tokens_for_bytes(note_text(summary, cut).len())
}

fn value_tokens(v: &Value, limits: &Limits, in_result: bool) -> usize {
    match v {
        Value::String(s) => tokens_for_bytes(s.len()),
        Value::Array(a) => a.iter().map(|x| value_tokens(x, limits, in_result)).sum(),
        // No message-based target carries an image from a tool result: its copy is a note.
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("image") => {
            if in_result { attach::result_image_tokens() } else { attach::estimate(limits, v) }
        }
        Value::Object(o) => o.values().map(|x| value_tokens(x, limits, in_result)).sum(),
        _ => 1,
    }
}

/// Estimated tokens of one message as a target reads it: its content, tool calls and results.
fn msg_tokens(m: &Msg, limits: &Limits) -> usize {
    let in_result = m.role == "toolResult";
    TOKENS_PER_MESSAGE + m.message.values().map(|x| value_tokens(x, limits, in_result)).sum::<usize>()
}

/// Positions a window may start at: user messages where no tool result at or after the cut
/// answers a call before it. Ascending.
fn cut_points(msgs: &[Msg]) -> Vec<usize> {
    let n = msgs.len();
    let mut call_at: HashMap<&str, usize> = HashMap::new();
    let mut results: Vec<(usize, &str)> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        if m.role == "toolResult" {
            if let Some(id) = m.message.get("toolCallId").and_then(Value::as_str) {
                results.push((i, id));
            }
        }
        for b in &m.content {
            if b.get("type").and_then(Value::as_str) == Some("toolCall") {
                if let Some(id) = b.get("id").and_then(Value::as_str) {
                    call_at.insert(id, i);
                }
            }
        }
    }
    // A cut c separates a call at ci from its result at r when ci < c <= r: mark that span.
    let mut open = vec![0i64; n + 1];
    for (r, id) in results {
        if let Some(&ci) = call_at.get(id) {
            if ci < r {
                open[ci + 1] += 1;
                open[r + 1] -= 1;
            }
        }
    }
    let mut depth = 0i64;
    let mut out = Vec::new();
    for c in 0..n {
        depth += open[c];
        if depth == 0 && msgs[c].role == "user" {
            out.push(c);
        }
    }
    out
}

impl Journal {
    /// The live window with every message of it, nothing fitted. For the writers that never fit.
    pub fn window(&self) -> Window<'_> {
        // Not fitted for any one target: the strictest limits, so the estimate stays an upper bound.
        let limits = Limits::for_target("claude");
        let msgs = self.messages();
        let summary = self.summary_of();
        let total = note_tokens(summary_text(&summary), false) + msgs.iter().map(|m| msg_tokens(m, limits)).sum::<usize>();
        let turns = msgs.iter().filter(|m| m.role == "user").count();
        Window {
            summary,
            cut: false,
            msgs,
            report: Report { turns, turns_kept: turns, tokens_before: total, tokens_after: total, limit: None },
        }
    }

    /// The live window cut to `budget`: the note, then the newest user turns that fit, starting at
    /// a user turn. Refuses when the note and the newest valid turn do not fit together.
    pub fn fit(&self, target: &str, budget: &Budget) -> Result<Window<'_>, String> {
        let limit = budget.limit();
        let limits = Limits::for_target(target);
        let all = self.messages();
        let summary = self.summary_of();
        let plain = note_tokens(summary_text(&summary), false);
        let costs: Vec<usize> = all.iter().map(|m| msg_tokens(m, limits)).collect();
        // suffix[c]: tokens of every message from c on.
        let mut suffix = vec![0usize; costs.len() + 1];
        for c in (0..costs.len()).rev() {
            suffix[c] = suffix[c + 1] + costs[c];
        }
        let total = plain + suffix[0];
        let turns = all.iter().filter(|m| m.role == "user").count();
        if total <= limit {
            return Ok(Window {
                summary,
                cut: false,
                msgs: all,
                report: Report { turns, turns_kept: turns, tokens_before: total, tokens_after: total, limit: Some(limit) },
            });
        }

        let with_cut = note_tokens(summary_text(&summary), true);
        let points = cut_points(&all);
        let Some(&newest) = points.last() else {
            return Err(format!(
                "context fit for {target}: no user turn keeps every tool call with its result; nothing was written"
            ));
        };
        let Some(&start) = points.iter().find(|&&c| with_cut + suffix[c] <= limit) else {
            return Err(format!(
                "context fit for {target}: the compaction summary and the newest turn need about {} tokens, \
                 over the {limit}-token limit ({}); nothing was written",
                with_cut + suffix[newest],
                budget.describe()
            ));
        };
        let mut all = all;
        let msgs = all.split_off(start);
        let turns_kept = msgs.iter().filter(|m| m.role == "user").count();
        Ok(Window {
            summary,
            cut: true,
            msgs,
            report: Report {
                turns,
                turns_kept,
                tokens_before: total,
                tokens_after: with_cut + suffix[start],
                limit: Some(limit),
            },
        })
    }

    /// The last compaction's summary and its time, owned.
    fn summary_of(&self) -> Option<(String, String)> {
        self.live_compaction().map(|(s, ts)| (s.to_string(), ts.to_string()))
    }
}

fn summary_text(summary: &Option<(String, String)>) -> Option<&str> {
    summary.as_ref().map(|(t, _)| t.as_str())
}

#[cfg(test)]
mod tests;
