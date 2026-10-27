//! The context window of each message-based target, and the flags that fit a copy to it.
//!
//! A copy written for Claude, OpenCode or Antigravity must fit that harness's window.
//! `sesync-conv` keeps the compaction summary and the newest turns that fit
//! `window x share - overhead` tokens (docs/ARCHITECTURE.md, "Context fit"). This module chooses
//! the window, first match wins:
//!
//! 1. `SESYNC_CONTEXT_WINDOW_<TARGET>`, then `SESYNC_CONTEXT_WINDOW` (tokens).
//! 2. OpenCode only: the model's `limit.context` in its config, `opencode.json` or `opencode.jsonc`
//!    under `$XDG_CONFIG_HOME/opencode` (else `~/.config/opencode`), for the provider and model the
//!    journal's last assistant turn names. Comments are stripped; trailing commas are not
//!    accepted, and a file that does not parse is skipped.
//! 3. [`TABLE`]: built-in windows, keyed by harness and an optional model prefix.
//! 4. [`CONSERVATIVE_WINDOW`] when nothing above is known.
//!
//! `SESYNC_CONTEXT_SHARE` (above 0, at most 1; default 0.75) and `SESYNC_CONTEXT_OVERHEAD` (tokens;
//! default 16384) apply to every target. A value that does not parse is ignored, never fatal.

use super::journal::tail_lines;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The window used when no source names one: the smallest window in the table, so an unknown
/// target is fitted for the harness that has the least room.
pub const CONSERVATIVE_WINDOW: usize = 128_000;
pub const DEFAULT_SHARE: f64 = 0.75;
pub const DEFAULT_OVERHEAD: usize = 16_384;

/// Built-in windows: (harness id, model prefix, tokens). An empty prefix matches any model.
/// `claude`: Anthropic's standard 200k context window, which the Claude models Claude Code runs
/// share. A model-specific row goes here when its window is published and differs.
const TABLE: &[(&str, &str, usize)] = &[("claude", "", 200_000)];

/// The `--context-*` flags `sesync-conv` takes for a copy written to `harness` from `journal`,
/// with the window read from the process environment and the home directory (`home::home_dir`).
pub fn flags(harness: &str, journal: &Path) -> Vec<String> {
    let home = Some(crate::home::home_dir()).filter(|h| !h.as_os_str().is_empty());
    flags_with(harness, journal, &|k| std::env::var(k).ok(), home.as_deref())
}

/// [`flags`] with the environment and home passed in: the same rules, testable without touching
/// the process environment.
pub fn flags_with(harness: &str, journal: &Path, env: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> Vec<String> {
    let (window, _) = window_for(harness, journal, env, home);
    let share = env("SESYNC_CONTEXT_SHARE").and_then(|v| v.trim().parse::<f64>().ok()).filter(|s| *s > 0.0 && *s <= 1.0).unwrap_or(DEFAULT_SHARE);
    let overhead = env("SESYNC_CONTEXT_OVERHEAD").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(DEFAULT_OVERHEAD);
    vec![
        "--context-window".into(), window.to_string(),
        "--context-share".into(), share.to_string(),
        "--context-overhead".into(), overhead.to_string(),
    ]
}

/// The window a target gets, and where it came from (for diagnostics and tests).
pub fn window_for(harness: &str, journal: &Path, env: &dyn Fn(&str) -> Option<String>, home: Option<&Path>) -> (usize, String) {
    let positive = |v: Option<String>| v.and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n > 0);
    let per_target = format!("SESYNC_CONTEXT_WINDOW_{}", harness.to_ascii_uppercase());
    if let Some(n) = positive(env(&per_target)) {
        return (n, per_target);
    }
    if let Some(n) = positive(env("SESYNC_CONTEXT_WINDOW")) {
        return (n, "SESYNC_CONTEXT_WINDOW".into());
    }
    let model = last_model(journal);
    if harness == "opencode" {
        if let Some((provider, model)) = model.as_ref() {
            let config_home = env("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
                .or_else(|| home.map(|h| h.join(".config")));
            if let Some(n) = config_home.and_then(|c| opencode_limit(&c, provider, model)) {
                return (n, format!("OpenCode config ({provider}/{model})"));
            }
        }
    }
    let named = model.as_ref().map(|(_, m)| m.as_str()).unwrap_or("");
    if let Some((_, _, n)) = TABLE.iter().find(|(h, prefix, _)| *h == harness && named.starts_with(prefix)) {
        return (*n, format!("built-in table ({harness})"));
    }
    (CONSERVATIVE_WINDOW, "conservative default".into())
}

/// The provider and model the journal's last assistant turn names, if it names one. Only the
/// end of the file is read, as the preview does. Journals converted from Claude carry the model;
/// those whose source does not name one give None.
pub fn last_model(journal: &Path) -> Option<(String, String)> {
    let lines = tail_lines(journal, 8, |l| l.contains("\"assistant\"")).ok()?;
    lines.iter().rev().find_map(|l| {
        let m = serde_json::from_str::<Value>(l).ok()?;
        let msg = m.get("message")?;
        if msg.get("role")?.as_str()? != "assistant" {
            return None;
        }
        let model = msg.get("model")?.as_str()?.to_string();
        let provider = msg.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
        Some((provider, model))
    })
}

/// `provider.<provider>.models.<model>.limit.context` from OpenCode's config under `config_home`.
fn opencode_limit(config_home: &Path, provider: &str, model: &str) -> Option<usize> {
    let dir = config_home.join("opencode");
    ["opencode.json", "opencode.jsonc"].iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let v = serde_json::from_str::<Value>(&strip_comments(&text)).ok()?;
        let n = v.get("provider")?.get(provider)?.get("models")?.get(model)?.pointer("/limit/context")?.as_u64()?;
        usize::try_from(n).ok().filter(|n| *n > 0)
    })
}

/// `text` with `//` and `/* */` comments removed outside strings.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn strip_comments_keeps_strings_that_look_like_comments() {
        let text = "{\n // a line comment\n \"url\": \"http://x/*y*/\", /* block */ \"n\": 1\n}";
        let v: Value = serde_json::from_str(&strip_comments(text)).unwrap();
        assert_eq!(v["url"], "http://x/*y*/");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let journal = Path::new("/nonexistent/journal.jsonl");
        assert_eq!(
            flags_with("claude", journal, &no_env, None),
            ["--context-window", "200000", "--context-share", "0.75", "--context-overhead", "16384"]
        );
        assert_eq!(window_for("antigravity", journal, &no_env, None).0, CONSERVATIVE_WINDOW);
        assert_eq!(window_for("opencode", journal, &no_env, None).0, CONSERVATIVE_WINDOW, "no config, no model: conservative");
    }

    #[test]
    fn overrides_win_and_bad_values_are_ignored() {
        let journal = Path::new("/nonexistent/journal.jsonl");
        let env = |k: &str| match k {
            "SESYNC_CONTEXT_WINDOW_OPENCODE" => Some("64000".to_string()),
            "SESYNC_CONTEXT_WINDOW" => Some("999".to_string()),
            "SESYNC_CONTEXT_SHARE" => Some("1.5".to_string()),
            "SESYNC_CONTEXT_OVERHEAD" => Some("nope".to_string()),
            _ => None,
        };
        assert_eq!(window_for("opencode", journal, &env, None), (64_000, "SESYNC_CONTEXT_WINDOW_OPENCODE".into()));
        assert_eq!(window_for("claude", journal, &env, None).0, 999, "the global override applies to every target");
        let f = flags_with("claude", journal, &env, None);
        assert_eq!(f[3], "0.75", "a share above 1 is ignored");
        assert_eq!(f[5], "16384", "an overhead that does not parse is ignored");
        let zero = |k: &str| (k == "SESYNC_CONTEXT_WINDOW").then(|| "0".to_string());
        assert_eq!(window_for("claude", journal, &zero, None).0, 200_000, "a zero window is not a window");
    }

    #[test]
    fn opencode_window_comes_from_its_config_for_the_journal_model() {
        let dir = std::env::temp_dir().join(format!("sesync-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config/opencode")).unwrap();
        std::fs::write(
            dir.join(".config/opencode/opencode.jsonc"),
            "{\n  // local models\n  \"provider\": { \"ollama\": { \"models\": {\n    \"qwen3\": { \"limit\": { \"context\": 262144, \"output\": 8192 } }\n  } } }\n}\n",
        )
        .unwrap();
        let journal = dir.join("canonical.jsonl");
        std::fs::write(
            &journal,
            concat!(
                r#"{"type":"session","id":"s","cwd":"/w"}"#, "\n",
                r#"{"type":"message","id":"a1","parentId":null,"message":{"role":"assistant","provider":"ollama","model":"qwen3","content":[{"type":"text","text":"hi"}]}}"#, "\n",
            ),
        )
        .unwrap();
        assert_eq!(last_model(&journal), Some(("ollama".into(), "qwen3".into())));
        let (n, source) = window_for("opencode", &journal, &no_env, Some(&dir));
        assert_eq!(n, 262_144);
        assert_eq!(source, "OpenCode config (ollama/qwen3)");
        let (other, _) = window_for("opencode", &journal, &|k| (k == "XDG_CONFIG_HOME").then(|| dir.join("elsewhere").display().to_string()), Some(&dir));
        assert_eq!(other, CONSERVATIVE_WINDOW, "XDG_CONFIG_HOME replaces ~/.config");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
