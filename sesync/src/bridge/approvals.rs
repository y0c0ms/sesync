//! Approvals and questions a terminal-backed agent is waiting on, read from the pane's screen. Herdr
//! reports a pane `blocked` while its agent shows one of these prompts, and only when the harness's herdr
//! integration is installed (omp: `herdr integration install omp`; OpenCode: `herdr integration install
//! opencode`). Without it an omp approval stays `idle`. This module reads the prompt off the screen so the
//! phone can show it, and names the keys that answer it.
//!
//! Every function is pure over screen text, so the tests run on captures from real harnesses
//! (`tests/fixtures/approvals/`). Verified keys (herdr 0.9.1, omp 18.8.6, OpenCode 1.19.4):
//! * omp approval (`Approve` / `Deny`, arrow-driven): Enter approves, Escape denies. `1` does nothing.
//! * omp ask (`○` choices): Enter selects the highlighted choice. Digits do nothing.
//! * OpenCode permission (`Allow once` / `Allow always` / `Reject`): Enter approves, Escape rejects.
//! * OpenCode question (numbered choices): a digit selects that choice at once.
//! Claude Code is UNVERIFIED: no capture exists, because fake-model does not speak the Anthropic API.

use serde_json::{json, Value};

const ESCAPE: &str = "\u{1b}";
const ENTER: &str = "\r";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Harness {
    Omp,
    OpenCode,
}

impl Harness {
    /// The harness a herdr pane's `agent` names, when this module reads its prompts.
    fn of(agent: &str) -> Option<Harness> {
        match agent {
            "omp" => Some(Harness::Omp),
            "opencode" => Some(Harness::OpenCode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    label: String,
    description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Prompt {
    Approval { tool: String, summary: String },
    Question { tool: String, question: String, options: Vec<Choice> },
}

/// The JSON a status row's `agentStatus.interactivePrompt` carries for the prompt `agent` is waiting on:
/// `{"approval":{"tool","summary"?}}`, or `{"toolName","questions":[…]}` for a question. None when the
/// screen shows no prompt this module recognises for `agent` (Claude has no recognizer yet). The caller
/// gates on herdr's `blocked` state; this function does not check it.
pub fn interactive_prompt(screen: &str, agent: &str) -> Option<String> {
    let harness = Harness::of(agent)?;
    find(&plain_screen(screen), harness).map(|prompt| prompt_value(&prompt).to_string())
}

/// The keys that confirm the highlighted choice of an approval, sent as text with Enter off. A carriage
/// return confirms `Approve` (omp) and `Allow once` (OpenCode). Claude's numbered prompt is assumed to take
/// `1`; that is UNVERIFIED.
pub fn approve_key(agent: &str) -> &'static str {
    match agent {
        "claude" => "1",
        _ => ENTER,
    }
}

/// The key that rejects an approval: Escape. Verified for omp and OpenCode; UNVERIFIED for Claude.
pub fn deny_key(_agent: &str) -> &'static str {
    ESCAPE
}

/// True when the prompt on `before` is no longer on `after`: the screen shows no prompt, or a different
/// one. False when `before` shows no prompt (nothing was cleared) or `after` still shows the same prompt.
pub fn prompt_cleared(before: &str, after: &str) -> bool {
    match find_any(&plain_screen(before)) {
        None => false,
        Some(shown) => match find_any(&plain_screen(after)) {
            None => true,
            Some(now) => now != shown,
        },
    }
}

/// The screen's text without terminal escape sequences (CSI, OSC and two-byte escapes) and carriage
/// returns. herdr's `format: ansi` reads contain them; the recognizers need plain rows.
pub fn plain_screen(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameter bytes, then one final byte in `@`..=`~`.
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: text up to BEL or ESC \.
                Some(']') => {
                    let mut prev = '\0';
                    for n in chars.by_ref() {
                        if n == '\u{7}' || (prev == '\u{1b}' && n == '\\') {
                            break;
                        }
                        prev = n;
                    }
                }
                // Intermediate bytes (0x20..=0x2F, e.g. the charset designator `ESC ( B`), then one final byte.
                Some(c) if (' '..='/').contains(&c) => {
                    for n in chars.by_ref() {
                        if !(' '..='/').contains(&n) {
                            break;
                        }
                    }
                }
                // A one-byte escape (`ESC c`, `ESC 7`): the byte after ESC is dropped.
                _ => {}
            },
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

fn find_any(screen: &str) -> Option<Prompt> {
    find(screen, Harness::Omp).or_else(|| find(screen, Harness::OpenCode))
}

fn find(screen: &str, harness: Harness) -> Option<Prompt> {
    let lines: Vec<&str> = screen.lines().collect();
    match harness {
        Harness::Omp => omp_approval(&lines).or_else(|| omp_ask(&lines)),
        Harness::OpenCode => opencode_approval(&lines).or_else(|| opencode_question(&lines)),
    }
}

/// The rows of a box with `│` sides, from the line below its top edge down to its bottom edge `╰`.
/// Separator rows (`├…┤`) are kept so callers can skip them. None when the bottom edge is not on screen.
fn box_rows(below_top: &[&str]) -> Option<Vec<String>> {
    let end = below_top.iter().position(|line| line.trim_start().starts_with('╰'))?;
    Some(
        below_top[..end]
            .iter()
            .map(|line| line.trim().trim_start_matches('│').trim_end_matches('│').trim().to_string())
            .collect(),
    )
}

/// One row of OpenCode's `┃`-gutter layout, without the gutter.
fn gutter_row(line: &str) -> &str {
    line.trim().trim_start_matches('┃').trim()
}

/// The label of a numbered choice row (`2. Blue`).
fn numbered(row: &str) -> Option<&str> {
    let (number, label) = row.split_once(". ")?;
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit())).then_some(label.trim())
}

const OMP_APPROVAL_TOP: &str = "╭─ Allow tool: ";
const OMP_APPROVAL_CHOICES: [&str; 2] = ["Approve", "Deny"];

/// omp's bash approval box: `╭─ Allow tool: bash`, the command as body text, then `Approve` / `Deny`.
fn omp_approval(lines: &[&str]) -> Option<Prompt> {
    let top = lines.iter().rposition(|line| line.trim_start().starts_with(OMP_APPROVAL_TOP))?;
    let tool = lines[top].trim_start().strip_prefix(OMP_APPROVAL_TOP)?.split_whitespace().next()?.to_string();
    let rows = box_rows(&lines[top + 1..])?;
    let mut summary: Vec<&str> = Vec::new();
    let mut choices: Vec<&str> = Vec::new();
    for row in &rows {
        let label = row.trim_start_matches(|c: char| c == '❯' || c.is_whitespace());
        if OMP_APPROVAL_CHOICES.iter().any(|choice| *choice == label) {
            choices.push(label);
        } else if choices.is_empty() && !row.is_empty() && !row.contains("navigate") {
            summary.push(row.as_str());
        }
    }
    (choices.len() == OMP_APPROVAL_CHOICES.len())
        .then(|| Prompt::Approval { tool, summary: summary.join("\n") })
}

const OMP_ASK_TOP: &str = "╭─ Ask ";
const OMP_ASK_FOOTER: &str = "⏎ select";
const OMP_ASK_OTHER: &str = "Other (type your own)";

/// omp's live question box: `╭─ Ask `, the question, `○` choices, and the `⏎ select` footer. The
/// transcript's earlier `╭─── Ask 1 questions` box has no such footer and a different top edge.
fn omp_ask(lines: &[&str]) -> Option<Prompt> {
    let top = lines.iter().rposition(|line| line.trim_start().starts_with(OMP_ASK_TOP))?;
    let rows = box_rows(&lines[top + 1..])?;
    if !rows.iter().any(|row| row.contains(OMP_ASK_FOOTER)) {
        return None;
    }
    let question = rows.iter().find(|row| !row.is_empty() && !row.starts_with('├'))?.clone();
    let mut options = Vec::new();
    for row in &rows {
        let text = row.trim_start_matches(|c: char| c == '❯' || c.is_whitespace());
        let label = text.strip_prefix("○ ").or_else(|| text.strip_prefix("◉ ")).map(str::trim);
        if let Some(label) = label.filter(|label| !label.is_empty() && *label != OMP_ASK_OTHER) {
            options.push(Choice { label: label.to_string(), description: None });
        }
    }
    (!options.is_empty()).then(|| Prompt::Question { tool: "ask".into(), question, options })
}

const OPENCODE_PERMISSION: &str = "△ Permission required";
const OPENCODE_ALLOW_ONCE: &str = "Allow once";
const OPENCODE_REJECT: &str = "Reject";

/// OpenCode's permission block: `△ Permission required`, a `# <kind>` heading, the request as body text,
/// then `Allow once` / `Allow always` / `Reject`.
fn opencode_approval(lines: &[&str]) -> Option<Prompt> {
    let top = lines.iter().rposition(|line| gutter_row(line).starts_with(OPENCODE_PERMISSION))?;
    let mut tool: Option<String> = None;
    let mut summary: Vec<&str> = Vec::new();
    for &line in &lines[top + 1..] {
        let row = gutter_row(line);
        if row.contains(OPENCODE_ALLOW_ONCE) && row.contains(OPENCODE_REJECT) {
            return Some(Prompt::Approval {
                tool: tool.unwrap_or_else(|| "permission".into()),
                summary: summary.join("\n"),
            });
        }
        if let Some(heading) = row.strip_prefix("# ") {
            tool = Some(opencode_tool(heading));
        } else if !row.is_empty() {
            summary.push(row);
        }
    }
    None
}

/// The tool a permission heading names. OpenCode titles a shell permission `# Shell command`.
fn opencode_tool(heading: &str) -> String {
    match heading {
        "Shell command" => "bash".to_string(),
        other => other.to_lowercase().replace(' ', "_"),
    }
}

const OPENCODE_ASK_FOOTER: &str = "enter submit";
const OPENCODE_ASK_OTHER: &str = "Type your own answer";

/// OpenCode's question block: the question, numbered choices with an optional description line each, and
/// the `enter submit` footer. The block starts after the last transcript marker (`▣`) above the footer.
fn opencode_question(lines: &[&str]) -> Option<Prompt> {
    let footer = lines.iter().rposition(|line| line.contains(OPENCODE_ASK_FOOTER))?;
    let start = lines[..footer].iter().rposition(|line| line.contains('▣')).map_or(0, |i| i + 1);
    let mut rows = lines[start..footer].iter().map(|&line| gutter_row(line)).filter(|row| !row.is_empty());
    let question = rows.next()?.to_string();
    let mut options: Vec<Choice> = Vec::new();
    // Whether the next unnumbered row describes the last choice.
    let mut open_choice = false;
    for row in rows {
        match numbered(row) {
            Some(label) if label == OPENCODE_ASK_OTHER => open_choice = false,
            Some(label) => {
                options.push(Choice { label: label.to_string(), description: None });
                open_choice = true;
            }
            None if open_choice => {
                if let Some(last) = options.last_mut() {
                    last.description = Some(row.to_string());
                }
                open_choice = false;
            }
            None => {}
        }
    }
    (!options.is_empty()).then(|| Prompt::Question { tool: "question".into(), question, options })
}

fn prompt_value(prompt: &Prompt) -> Value {
    match prompt {
        Prompt::Approval { tool, summary } => {
            let mut approval = json!({ "tool": tool });
            if !summary.is_empty() {
                approval["summary"] = json!(summary);
            }
            json!({ "approval": approval })
        }
        Prompt::Question { tool, question, options } => {
            let options: Vec<Value> = options.iter().map(choice_value).collect();
            json!({
                "toolName": tool,
                "questions": [{ "question": question, "multiSelect": false, "options": options }],
            })
        }
    }
}

fn choice_value(choice: &Choice) -> Value {
    match &choice.description {
        Some(description) => json!({ "label": choice.label, "description": description }),
        None => json!({ "label": choice.label }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OMP_APPROVAL: &str = include_str!("../../tests/fixtures/approvals/omp-approval.txt");
    const OMP_APPROVAL_ANSI: &str = include_str!("../../tests/fixtures/approvals/omp-approval-ansi.txt");
    const OMP_APPROVED: &str = include_str!("../../tests/fixtures/approvals/omp-approval-after-approve.txt");
    const OMP_QUESTION: &str = include_str!("../../tests/fixtures/approvals/omp-question.txt");
    const OMP_ANSWERED: &str = include_str!("../../tests/fixtures/approvals/omp-question-after-answer.txt");
    const OPENCODE_APPROVAL: &str = include_str!("../../tests/fixtures/approvals/opencode-approval.txt");
    const OPENCODE_APPROVAL_ANSI: &str = include_str!("../../tests/fixtures/approvals/opencode-approval-ansi.txt");
    const OPENCODE_REJECTED: &str = include_str!("../../tests/fixtures/approvals/opencode-approval-after-reject.txt");
    const OPENCODE_QUESTION: &str = include_str!("../../tests/fixtures/approvals/opencode-question.txt");
    const OPENCODE_ANSWERED: &str = include_str!("../../tests/fixtures/approvals/opencode-question-after-answer.txt");

    fn read(screen: &str, agent: &str) -> Value {
        let text = interactive_prompt(screen, agent).expect("the screen shows a prompt");
        serde_json::from_str(&text).expect("the prompt is JSON")
    }

    #[test]
    fn the_omp_approval_capture_reads_as_a_bash_approval() {
        assert_eq!(
            read(OMP_APPROVAL, "omp"),
            json!({ "approval": { "tool": "bash", "summary": "Command: ls -la" } })
        );
    }

    #[test]
    fn the_opencode_approval_capture_reads_as_a_bash_approval() {
        assert_eq!(
            read(OPENCODE_APPROVAL, "opencode"),
            json!({ "approval": { "tool": "bash", "summary": "$ ls -la" } })
        );
    }

    #[test]
    fn the_omp_ask_capture_reads_as_a_single_question_with_its_choices() {
        assert_eq!(
            read(OMP_QUESTION, "omp"),
            json!({
                "toolName": "ask",
                "questions": [{
                    "question": "Which colour?",
                    "multiSelect": false,
                    "options": [{ "label": "Red" }, { "label": "Blue" }]
                }]
            })
        );
    }

    #[test]
    fn the_opencode_question_capture_reads_its_choices_and_descriptions() {
        assert_eq!(
            read(OPENCODE_QUESTION, "opencode"),
            json!({
                "toolName": "question",
                "questions": [{
                    "question": "Which colour?",
                    "multiSelect": false,
                    "options": [
                        { "label": "Red", "description": "warm" },
                        { "label": "Blue", "description": "cool" }
                    ]
                }]
            })
        );
    }

    #[test]
    fn ansi_screens_read_the_same_as_their_plain_text() {
        assert_eq!(read(OMP_APPROVAL_ANSI, "omp"), read(OMP_APPROVAL, "omp"));
        assert_eq!(read(OPENCODE_APPROVAL_ANSI, "opencode"), read(OPENCODE_APPROVAL, "opencode"));
    }

    #[test]
    fn settled_screens_show_no_prompt() {
        assert_eq!(interactive_prompt(OMP_APPROVED, "omp"), None);
        assert_eq!(interactive_prompt(OMP_ANSWERED, "omp"), None, "the answered ask stays in the transcript");
        assert_eq!(interactive_prompt(OPENCODE_REJECTED, "opencode"), None);
        assert_eq!(interactive_prompt(OPENCODE_ANSWERED, "opencode"), None);
    }

    #[test]
    fn a_prompt_is_read_only_for_the_harness_that_draws_it() {
        assert_eq!(interactive_prompt(OMP_APPROVAL, "opencode"), None);
        assert_eq!(interactive_prompt(OPENCODE_QUESTION, "omp"), None);
        assert_eq!(interactive_prompt(OMP_APPROVAL, "claude"), None, "claude has no capture, so no recognizer");
        assert_eq!(interactive_prompt(OMP_APPROVAL, ""), None);
    }

    #[test]
    fn keys_follow_the_verified_captures() {
        assert_eq!(approve_key("omp"), "\r");
        assert_eq!(approve_key("opencode"), "\r");
        assert_eq!(approve_key("claude"), "1");
        assert_eq!(deny_key("omp"), "\u{1b}");
        assert_eq!(deny_key("opencode"), "\u{1b}");
        assert_eq!(deny_key("claude"), "\u{1b}");
    }

    #[test]
    fn a_prompt_is_cleared_once_the_screen_no_longer_shows_it() {
        assert!(prompt_cleared(OMP_APPROVAL, OMP_APPROVED));
        assert!(prompt_cleared(OMP_QUESTION, OMP_ANSWERED));
        assert!(prompt_cleared(OPENCODE_APPROVAL, OPENCODE_REJECTED));
        assert!(prompt_cleared(OPENCODE_QUESTION, OPENCODE_ANSWERED));
        assert!(prompt_cleared(OMP_APPROVAL, OMP_QUESTION), "a different prompt is not the same one");
        assert!(!prompt_cleared(OMP_APPROVAL, OMP_APPROVAL), "the same prompt is still shown");
        assert!(!prompt_cleared(OMP_APPROVED, OMP_APPROVED), "no prompt before, so nothing was cleared");
    }

    #[test]
    fn escape_sequences_are_removed_and_text_is_kept() {
        assert_eq!(plain_screen("\u{1b}[1;31mred\u{1b}[0m\r\n"), "red\n");
        assert_eq!(plain_screen("a\u{1b}]0;title\u{7}b"), "ab");
        assert!(!plain_screen(OPENCODE_APPROVAL_ANSI).contains('\u{1b}'));
    }
}
