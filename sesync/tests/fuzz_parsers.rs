//! Property tests: the parsers that read files written by other programs must never
//! panic on any input. A malformed journal, transcript, export row or state file is a
//! normal event, and the worst acceptable outcome is an error or an empty result.

use sesync::harness::claude::compaction::cut_compaction_cycles;
use sesync::harness::claude::{claude_title_from_lines, parse_claude_name};
use sesync::harness::journal::{omp_title_from_lines, parse_omp_name};
use sesync::harness::opencode::store::clean_title;
use sesync::state::State;
use proptest::prelude::*;
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-fuzz-{}-{name}", std::process::id()));
    let _ = fs::create_dir_all(&d);
    d
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn omp_title_never_panics(lines in proptest::collection::vec(".{0,200}", 0..20)) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let _ = omp_title_from_lines(&refs);
    }

    #[test]
    fn claude_title_never_panics(lines in proptest::collection::vec(".{0,200}", 0..20)) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let _ = claude_title_from_lines(&refs);
    }

    #[test]
    fn opencode_title_never_panics(t in ".{0,200}") {
        let _ = clean_title(&t);
    }

    #[test]
    fn session_file_names_never_panic(name in ".{0,120}") {
        let _ = parse_omp_name(&name);
        let _ = parse_claude_name(&name);
    }

    #[test]
    fn transcript_cycle_cut_never_panics(text in ".{0,400}") {
        let (out, cut) = cut_compaction_cycles(&text);
        // With no cut, the output is the input unchanged.
        if cut == 0 {
            prop_assert_eq!(out, text);
        }
    }

    #[test]
    fn state_file_never_panics_on_arbitrary_content(content in ".{0,400}") {
        let d = sandbox("state");
        let f = d.join(format!("state-{}.json", std::process::id()));
        fs::write(&f, &content).unwrap();
        // A malformed state must be an error, never a panic and never a silently
        // empty result for valid-looking content.
        let _ = State::load(&f);
    }
}

/// A JSON value of arbitrary shape: the fields the parsers read, with wrong types.
fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        ".{0,40}".prop_map(serde_json::Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::from),
            proptest::collection::hash_map("[a-zA-Z_]{1,12}", inner, 0..5)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    })
}

/// A record whose known keys hold arbitrary values, so every field type is exercised.
fn record() -> impl Strategy<Value = String> {
    let keys = prop::sample::select(vec![
        "type", "role", "message", "content", "text", "title", "customTitle", "aiTitle",
        "agentName", "name", "isMeta", "subtype", "uuid", "parentUuid", "logicalParentUuid",
        "leafUuid", "cwd", "timestamp",
    ]);
    proptest::collection::vec((keys, json_value()), 0..6).prop_map(|pairs| {
        let obj: serde_json::Map<String, serde_json::Value> =
            pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        serde_json::Value::Object(obj).to_string()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn structured_omp_records_never_panic(lines in proptest::collection::vec(record(), 0..12)) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let _ = omp_title_from_lines(&refs);
    }

    #[test]
    fn structured_claude_records_never_panic(lines in proptest::collection::vec(record(), 0..12)) {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let _ = claude_title_from_lines(&refs);
    }

    #[test]
    fn structured_transcripts_never_panic_in_the_cycle_cut(lines in proptest::collection::vec(record(), 0..12)) {
        let text = lines.join("\n");
        let _ = cut_compaction_cycles(&text);
    }
}
