//! Claude-as-source, ported from daemon/sesync.py sanitize_claude. The
//! converter rejects a Claude transcript whose compaction boundaries point into
//! each other ("ancestry cycle"). The cut must leave a readable transcript and
//! must not touch the original file.

use sesync::harness::claude::compaction::{cut_compaction_cycles, sanitize_claude};
use std::fs;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("hs-claude-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Two compact_boundary records whose logicalParentUuid links point at each other.
fn cyclic_transcript() -> String {
    [
        r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"s","message":{"role":"user","content":"hi"}}"#,
        r#"{"type":"system","subtype":"compact_boundary","uuid":"b1","parentUuid":null,"logicalParentUuid":"b2","sessionId":"s"}"#,
        r#"{"type":"system","subtype":"compact_boundary","uuid":"b2","parentUuid":null,"logicalParentUuid":"b1","sessionId":"s"}"#,
        r#"{"type":"assistant","uuid":"a1","parentUuid":"b2","sessionId":"s","message":{"role":"assistant","content":"ok"}}"#,
        r#"{"type":"last-prompt","leafUuid":"a1"}"#,
    ].join("\n") + "\n"
}

#[test]
fn a_cycle_is_cut_and_the_cut_count_is_reported() {
    let (out, cut) = cut_compaction_cycles(&cyclic_transcript());
    assert!(cut >= 1, "a cyclic transcript must have at least one link cut");
    // After the cut, following logicalParentUuid from any boundary must terminate.
    let mut seen = std::collections::HashSet::new();
    for line in out.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["subtype"] == "compact_boundary" {
            let mut cur = v["uuid"].as_str().unwrap().to_string();
            let mut steps = 0;
            loop {
                let rec = out.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                    .find(|r| r["uuid"] == cur);
                let Some(r) = rec else { break };
                let Some(next) = r["logicalParentUuid"].as_str().map(str::to_string) else { break };
                steps += 1;
                assert!(steps < 10, "logicalParentUuid walk did not terminate");
                cur = next;
            }
        }
        seen.insert(v["uuid"].as_str().unwrap_or("").to_string());
    }
}

#[test]
fn records_after_the_cut_are_kept() {
    let (out, _) = cut_compaction_cycles(&cyclic_transcript());
    assert!(out.contains("\"uuid\":\"a1\""), "the assistant reply after the boundary must survive");
    assert!(out.contains("\"uuid\":\"u1\""), "the first user message must survive");
}

#[test]
fn a_clean_transcript_is_unchanged() {
    let clean = "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":null,\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
    let (out, cut) = cut_compaction_cycles(clean);
    assert_eq!(cut, 0);
    assert_eq!(out, clean);
}

#[test]
fn sanitize_writes_a_copy_and_leaves_the_original_alone() {
    let d = sandbox("copy");
    let src = d.join("t.jsonl");
    let dst = d.join("clean.jsonl");
    fs::write(&src, cyclic_transcript()).unwrap();
    let cut = sanitize_claude(&src, &dst).unwrap();
    assert!(cut >= 1);
    assert_eq!(fs::read_to_string(&src).unwrap(), cyclic_transcript(), "the original must not change");
    assert!(dst.exists());
}
