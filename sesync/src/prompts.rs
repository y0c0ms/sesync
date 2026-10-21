//! When the user last typed into a conversation copy.
//!
//! Turn-taking moves a conversation's source to a copy only when someone answered inside that
//! copy. Bytes changing is not that signal: a rename, a resume that appends bookkeeping records,
//! a file rewritten by an older binary, or a state record that fell behind all change bytes with
//! nobody typing anything. A real answer leaves a user prompt stamped later than every prompt of
//! the source, because every copy sesync writes carries the source's own timestamps.

/// The note the converters put in front of a compacted conversation. It is a user message by
/// shape, stamped with the compaction's time, but sesync wrote it: it is never a prompt.
pub const COMPACTION_NOTE_PREFIX: &str = "Earlier turns of this conversation were compacted.";

/// Seconds since the epoch of an RFC 3339 timestamp such as `2026-10-09T18:15:51.408Z` or
/// `2026-10-09T19:15:51+01:00`. None for anything else.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let b = s.trim().as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        std::str::from_utf8(&b[from..to]).ok()?.parse::<i64>().ok()
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac = 0.0;
    if i < b.len() && b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = std::str::from_utf8(&b[start..i]).ok()?;
        if !digits.is_empty() {
            frac = format!("0.{digits}").parse::<f64>().ok()?;
        }
    }
    let offset = match b.get(i) {
        None => 0,
        Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            if b.len() < i + 6 || b[i + 3] != b':' {
                return None;
            }
            let secs = num(i + 1, i + 3)? * 3600 + num(i + 4, i + 6)? * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    // Days from the civil date (proleptic Gregorian), so no calendar dependency is needed.
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset) as f64 + frac)
}

/// Whether a piece of text is something a person typed: not empty, not a command wrapper
/// (`<command-name>`, `<local-command-stdout>`) and not sesync's compaction note.
pub fn is_typed_text(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty() && !t.starts_with('<') && !t.starts_with(COMPACTION_NOTE_PREFIX)
}

/// Whether message content holds something a person typed (`is_typed_text`). Tool results and
/// image-only content are not prompts. Each journal harness applies it to its own record shape.
pub fn is_prompt_content(content: &serde_json::Value) -> bool {
    let typed = |t: &str| is_typed_text(t);
    match content {
        serde_json::Value::String(s) => typed(s),
        serde_json::Value::Array(blocks) => blocks.iter().any(|b| {
            let kind = b.get("type").and_then(serde_json::Value::as_str);
            (kind.is_none() || kind == Some("text"))
                && b.get("text").and_then(serde_json::Value::as_str).is_some_and(typed)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_offsets_and_fractions() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_rfc3339("2026-10-19T00:00:00.000Z"), Some(1_792_368_000.0));
        assert_eq!(parse_rfc3339("2026-10-19T01:00:00+01:00"), Some(1_792_368_000.0), "an offset is folded into UTC");
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00.5Z"), Some(951_825_600.5), "a leap day and a fraction");
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None, "month 13 is no date");
    }

    #[test]
    fn only_typed_text_is_a_prompt() {
        use serde_json::json;
        assert!(is_prompt_content(&json!("fix the bug")));
        assert!(is_prompt_content(&json!([{"type": "text", "text": "fix it"}])));
        assert!(is_prompt_content(&json!([{"text": "parts without a type are text"}])));
        assert!(!is_prompt_content(&json!("<command-name>/rename</command-name>")), "a command wrapper is not typed text");
        assert!(!is_prompt_content(&json!([{"type": "tool_result", "content": "ok"}])), "a tool result is not a prompt");
        assert!(!is_prompt_content(&json!([{"type": "text", "text": "Earlier turns of this conversation were compacted. Their summary:\n\nx"}])),
            "sesync's own compaction note is not a prompt");
        assert!(!is_prompt_content(&json!("   ")));
    }
}
