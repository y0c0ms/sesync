//! Claude Code transcripts as a source. Ported from daemon/sesync.py
//! sanitize_claude. Claude Code can write two compaction boundaries whose
//! logicalParentUuid links point into each other. The converter treats that as
//! an ancestry cycle and refuses the file. Claude Code itself does not follow
//! those links on resume, so cutting one link is safe.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// Cut every logicalParentUuid link that closes a loop in the active chain.
/// Returns the transcript text and the number of links cut (0 means unchanged).
pub fn cut_compaction_cycles(text: &str) -> (String, usize) {
    let mut recs: Vec<Value> = text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() { None } else { serde_json::from_str::<Value>(l).ok() }
        })
        .collect();

    // uuid -> index into recs, so a closing boundary can be edited in place.
    let mut by: HashMap<String, usize> = HashMap::new();
    for (i, r) in recs.iter().enumerate() {
        if let Some(u) = r.get("uuid").and_then(Value::as_str) {
            by.insert(u.to_string(), i);
        }
    }
    let leaf = recs
        .iter()
        .rev()
        .find(|r| r.get("type").and_then(Value::as_str) == Some("last-prompt")
            && r.get("leafUuid").and_then(Value::as_str).is_some())
        .and_then(|r| r["leafUuid"].as_str().map(str::to_string))
        .or_else(|| {
            recs.iter().rev()
                .find(|r| matches!(r.get("type").and_then(Value::as_str), Some("user") | Some("assistant")))
                .and_then(|r| r.get("uuid").and_then(Value::as_str).map(str::to_string))
        });

    let mut cut = 0;
    let Some(leaf) = leaf else { return (text.to_string(), 0) };
    // Each pass must cut one link; there can be at most one cut per record, so
    // stop after that. Without the bound a cut that fails to stick would loop forever.
    for _ in 0..=recs.len() {
        // Walk the active chain from the leaf; remember the boundary that closes a loop.
        let mut seen: HashSet<String> = HashSet::new();
        let mut cur = Some(leaf.clone());
        let mut closing: Option<usize> = None;
        while let Some(c) = cur.take() {
            if !seen.insert(c.clone()) {
                break;
            }
            let Some(&i) = by.get(&c) else { break };
            let r = &recs[i];
            let mut nxt = r.get("parentUuid").and_then(Value::as_str).map(str::to_string);
            if nxt.is_none()
                && r.get("type").and_then(Value::as_str) == Some("system")
                && r.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            {
                nxt = r.get("logicalParentUuid").and_then(Value::as_str).map(str::to_string);
                if let Some(n) = &nxt {
                    if seen.contains(n) {
                        closing = Some(i);
                    }
                }
            }
            cur = nxt;
        }
        let Some(i) = closing else { break };
        recs[i]["logicalParentUuid"] = Value::Null;
        cut += 1;
    }

    if cut == 0 {
        return (text.to_string(), 0);
    }
    let mut out = String::new();
    for r in &recs {
        out.push_str(&r.to_string());
        out.push('\n');
    }
    (out, cut)
}

/// Write a cycle-free copy of `src` to `dst`. Returns the links cut. The original
/// is never modified, and `dst` is written only when a cut was needed.
pub fn sanitize_claude(src: &Path, dst: &Path) -> Result<usize, String> {
    let text = fs::read_to_string(src).map_err(|e| e.to_string())?;
    let (out, cut) = cut_compaction_cycles(&text);
    if cut > 0 {
        fs::write(dst, out).map_err(|e| e.to_string())?;
    }
    Ok(cut)
}
