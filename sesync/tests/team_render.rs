//! The Team tab as drawn on a TestBackend: the compact unified list with its five states, and the
//! divergence dialog side by side when wide and stacked when narrow. Each prints the screen (run with
//! `--nocapture`) and checks the layout it promises.

#[path = "support/team_list.rs"]
mod team_list;

use sesync::app::{Ev, Mode};
use std::time::Instant;
use team_list::*;

const OPTIONS: [&str; 5] = [
    "Take theirs, keep mine here",
    "Take theirs, keep mine on team",
    "Keep mine, keep theirs here",
    "Keep mine, keep theirs on team",
    "Decide later",
];

fn open_dialog(sb: &std::path::Path) -> sesync::app::App {
    let mut a = five_states(sb);
    select_team_row(&mut a, DIVERGED);
    a.handle(Ev::Enter, Instant::now());
    assert_eq!(a.mode(), Mode::Resolve);
    a
}

#[test]
fn the_unified_list_drawn_compact_with_all_five_states() {
    let sb = sandbox("render-list");
    let mut a = five_states(&sb);
    a.handle(Ev::Tab, Instant::now());
    let text = screen_text(&a, 120, 30);
    println!("== Team tab, unified list, 120x30 ==\n{text}");
    let rows: Vec<&str> = text.lines().collect();
    for (state, title) in [("= in step", "Same"), ("↑ waiting", "Ahead"), ("↓ behind", "Behind"),
        ("≠ diverged", "Diverged"), ("• nothing to send", "Shared"), ("↓ not here", "Team only")] {
        assert!(rows.iter().any(|l| l.contains(state) && l.contains(title)), "a {title} row reads '{state}':\n{text}");
    }
    assert!(text.contains("UPDATED"), "the list has its column header:\n{text}");
    assert!(!text.contains("c1c1c1c1"), "a title that is not doubled shows no id:\n{text}");
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn the_divergence_dialog_drawn_wide_with_both_sides_side_by_side() {
    let sb = sandbox("render-wide");
    let a = open_dialog(&sb);
    let text = screen_text(&a, 120, 36);
    println!("== divergence dialog, wide, 120x36 ==\n{text}");
    assert!(text.contains("Same history up to turn 1."), "{text}");
    let both = text.lines().any(|l| l.contains("This machine") && l.contains("Team copy"));
    assert!(both, "the two sides share a row when wide:\n{text}");
    for label in OPTIONS {
        assert!(text.contains(label), "option {label}:\n{text}");
    }
    assert!(text.contains("Forks install into omp"), "{text}");
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn the_divergence_dialog_drawn_narrow_with_the_sides_stacked() {
    let sb = sandbox("render-narrow");
    let a = open_dialog(&sb);
    let text = screen_text(&a, 72, 44);
    println!("== divergence dialog, narrow, 72x44 ==\n{text}");
    assert!(text.contains("Same history up to turn 1."), "{text}");
    let this_row = text.lines().position(|l| l.contains("This machine")).expect("this machine's side");
    let team_row = text.lines().position(|l| l.contains("Team copy")).expect("the team copy's side");
    assert!(team_row > this_row, "the team copy is stacked below this machine");
    assert!(!text.lines().any(|l| l.contains("This machine") && l.contains("Team copy")));
    for label in OPTIONS {
        assert!(text.contains(label), "option {label} is not cut off:\n{text}");
    }
    let _ = std::fs::remove_dir_all(&sb);
}
