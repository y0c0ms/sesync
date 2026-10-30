//! `sesync machine …`: keep the pairings this machine holds for other machines' bridges, list what each one
//! runs, and attach to a conversation's pane. Exit codes follow `sesync bridge`: 0 on success, 2 on a usage
//! mistake, 1 on a failure.

use super::store::{self, Machine};
use super::{attach, client};
use crate::bridge::offer::Offer;
use serde_json::{json, Value};
use std::path::Path;

const USAGE: &str = "usage: sesync machine add <name> <pairing link>   (from `sesync bridge pair --label <this machine>` there)
       sesync machine list [<name>]
       sesync machine attach <name> <conversation title or id>   (Ctrl-] detaches)
       sesync machine remove <name>";

enum Fail {
    Usage(String),
    Failed(String),
}

/// Run one `sesync machine` command. `path` is the machines file.
pub fn run(args: &[String], path: &Path) -> i32 {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match words.as_slice() {
        ["add", name, link] => add(path, name, link),
        ["list"] => list(path, None),
        ["list", name] => list(path, Some(name)),
        ["attach", name, query] => attach(path, name, query),
        ["remove", name] => remove(path, name),
        _ => Err(Fail::Usage("expected add, list, attach or remove with their arguments".into())),
    };
    match result {
        Ok(()) => 0,
        Err(Fail::Usage(message)) => {
            eprintln!("sesync machine: {message}\n{USAGE}");
            2
        }
        Err(Fail::Failed(message)) => {
            eprintln!("sesync machine: {message}");
            1
        }
    }
}

fn add(path: &Path, name: &str, link: &str) -> Result<(), Fail> {
    let offer = Offer::parse(link).map_err(Fail::Usage)?;
    let replaced = store::add(path, name, &offer).map_err(Fail::Usage)?;
    // Prove the pairing before the user relies on it.
    let machine = named(path, name)?;
    let checked = runtime()?.block_on(async {
        let (mut writer, mut reader) = client::connect(&machine).await?;
        client::call(&mut writer, &mut reader, "status.get", json!({})).await
    });
    let verb = if replaced { "replaced" } else { "added" };
    match checked {
        Ok(_) => println!("{verb} {name} ({}); it answers", offer.endpoint),
        Err(e) => println!("{verb} {name} ({}), but it does not answer yet: {e}", offer.endpoint),
    }
    Ok(())
}

fn attach(path: &Path, name: &str, query: &str) -> Result<(), Fail> {
    let machine = named(path, name)?;
    let message = runtime()?.block_on(attach::run(&machine, query)).map_err(Fail::Failed)?;
    println!("{message}");
    Ok(())
}

fn remove(path: &Path, name: &str) -> Result<(), Fail> {
    let machine = store::remove(path, name).map_err(Fail::Failed)?;
    println!("removed {}; its pairing stays valid there until `sesync bridge revoke` on {}", machine.name, machine.name);
    Ok(())
}

fn named(path: &Path, name: &str) -> Result<Machine, Fail> {
    store::load(path)
        .map_err(Fail::Failed)?
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| Fail::Usage(format!("no machine named {name:?}; add it with `sesync machine add`")))
}

fn runtime() -> Result<tokio::runtime::Runtime, Fail> {
    tokio::runtime::Runtime::new().map_err(|e| Fail::Failed(format!("cannot start the runtime: {e}")))
}

/// Each machine (or only `only`), then its conversations: state, title, harness, project, id.
fn list(path: &Path, only: Option<&str>) -> Result<(), Fail> {
    let machines: Vec<Machine> = match only {
        Some(name) => vec![named(path, name)?],
        None => store::load(path).map_err(Fail::Failed)?,
    };
    if machines.is_empty() {
        println!("no machines; add one with `sesync machine add <name> <pairing link>`");
        return Ok(());
    }
    let rt = runtime()?;
    for machine in &machines {
        println!("{}  {}", machine.name, machine.endpoint);
        let rows = rt.block_on(async {
            let (mut writer, mut reader) = client::connect(machine).await?;
            client::call(&mut writer, &mut reader, "worktree.ps", json!({})).await
        });
        match rows {
            Ok(list) => {
                for row in list["worktrees"].as_array().into_iter().flatten() {
                    println!("  {}", list_line(row));
                }
            }
            Err(e) => println!("  unreachable: {e}"),
        }
    }
    Ok(())
}

/// One conversation as `list` prints it: its state (the first open pane's, else `closed`), title, harness,
/// project directory and worktree id.
fn list_line(row: &Value) -> String {
    let state = row["agents"][0]["state"].as_str().unwrap_or("closed");
    format!(
        "{state:<8} {} {:<9} {}  {}",
        crate::session_cols::fit(row["displayName"].as_str().unwrap_or(""), 32),
        row["conversationAgent"].as_str().unwrap_or(""),
        row["path"].as_str().unwrap_or(""),
        row["worktreeId"].as_str().unwrap_or(""),
    )
}
