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
       sesync machine attach <name> <conversation title or id> [--open]   (Ctrl-] detaches)
       sesync machine remove <name>
       sesync machine endpoint <name> <ws://address> | --clear   (dial that machine here, kept over a new grant)
       sesync machine send <name> <file or folder>   (into that machine's inbox; a folder goes as a zip)
       sesync machine get <name> <path on that machine> [<folder here>]   (a folder comes as a zip; default folder: here)
       sesync machine prompt <name> <conversation title or id> <text | -> [--open] [--wait[=<seconds>]]
                                (a prompt turn for that conversation's agent; - reads the text from stdin;
                                 --open resumes a closed conversation there; --wait prints the answer, default 600 s)
       sesync machine read <name> <conversation title or id> [<count>]   (its last messages, default 10)";

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
        ["attach", name, query] => attach(path, name, query, false),
        ["attach", name, query, "--open"] => attach(path, name, query, true),
        ["remove", name] => remove(path, name),
        ["endpoint", name, "--clear"] => endpoint(path, name, None),
        ["endpoint", name, address] => endpoint(path, name, Some(address)),
        ["send", name, source] => send(path, name, source),
        ["get", name, remote] => get(path, name, remote, "."),
        ["get", name, remote, into] => get(path, name, remote, into),
        ["prompt", name, query, text, flags @ ..] => prompt(path, name, query, text, flags),
        ["read", name, query] => read(path, name, query, "10"),
        ["read", name, query, count] => read(path, name, query, count),
        _ => Err(Fail::Usage("expected add, list, attach, remove, endpoint, send, get, prompt or read with their arguments".into())),
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

fn endpoint(path: &Path, name: &str, address: Option<&str>) -> Result<(), Fail> {
    let machine = store::set_local_endpoint(path, name, address).map_err(Fail::Usage)?;
    match &machine.local_endpoint {
        Some(local) => println!("{name} is dialled at {local} from here (its pairing names {})", machine.endpoint),
        None => println!("{name} is dialled at {} again", machine.endpoint),
    }
    Ok(())
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

fn attach(path: &Path, name: &str, query: &str, open: bool) -> Result<(), Fail> {
    let machine = named(path, name)?;
    let message = runtime()?.block_on(attach::run(&machine, query, open)).map_err(Fail::Failed)?;
    println!("{message}");
    Ok(())
}

fn remove(path: &Path, name: &str) -> Result<(), Fail> {
    let machine = store::remove(path, name).map_err(Fail::Failed)?
        .ok_or_else(|| Fail::Failed(format!("no machine named {name:?}")))?;
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

/// `send`: a file, or a folder as a zip, into the other machine's inbox.
fn send(path: &Path, name: &str, source: &str) -> Result<(), Fail> {
    let machine = named(path, name)?;
    let done = runtime()?.block_on(super::send::send(&machine, Path::new(source))).map_err(Fail::Failed)?;
    println!("{}", super::send::describe_sent(&machine.name, &done));
    Ok(())
}

/// `get`: a file, or a folder as a zip, from the other machine into a folder here.
fn get(path: &Path, name: &str, remote: &str, into: &str) -> Result<(), Fail> {
    let machine = named(path, name)?;
    let done = runtime()?.block_on(super::send::get(&machine, remote, Path::new(into))).map_err(Fail::Failed)?;
    println!("{}", super::send::describe_got(&done));
    Ok(())
}

/// The flags `prompt` takes after its text: `--open`, and `--wait` with an optional `=<seconds>` (600 by default).
fn prompt_flags(flags: &[&str]) -> Result<(bool, Option<std::time::Duration>), Fail> {
    let mut open = false;
    let mut wait = None;
    for flag in flags {
        match *flag {
            "--open" => open = true,
            "--wait" => wait = Some(std::time::Duration::from_secs(600)),
            other => match other.strip_prefix("--wait=").map(str::parse::<u64>) {
                Some(Ok(seconds)) if seconds > 0 => wait = Some(std::time::Duration::from_secs(seconds)),
                Some(_) => return Err(Fail::Usage(format!("{other}: --wait takes a number of seconds above 0"))),
                None => return Err(Fail::Usage(format!("unknown flag {other}"))),
            },
        }
    }
    Ok((open, wait))
}

/// `prompt`: a prompt turn for a conversation's agent on that machine; with `--wait`, its answer printed.
fn prompt(path: &Path, name: &str, query: &str, text: &str, flags: &[&str]) -> Result<(), Fail> {
    use super::prompt::{self as p, Settled};
    let (open, wait) = prompt_flags(flags)?;
    let text = if text == "-" {
        let mut read = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut read).map_err(|e| Fail::Failed(format!("cannot read the prompt from stdin: {e}")))?;
        read
    } else {
        text.to_string()
    };
    let machine = named(path, name)?;
    runtime()?.block_on(async {
        let (sent, mut writer, mut reader) = p::send(&machine, query, &text, open).await.map_err(Fail::Failed)?;
        let how = if sent.resumed { "resumed it and sent the prompt" } else { "sent the prompt" };
        eprintln!("{how} to {} on {}", sent.title, machine.name);
        let Some(limit) = wait else { return Ok(()) };
        match p::settle(&mut writer, &mut reader, &sent, limit).await.map_err(Fail::Failed)? {
            Settled::Answered(answer) => {
                println!("{}", p::text_of(&answer));
                Ok(())
            }
            Settled::Blocked => Err(Fail::Failed(format!("{} on {} is waiting on a question in its pane; answer it there (sesync machine attach)", sent.title, machine.name))),
            Settled::TimedOut => Err(Fail::Failed(format!("no answer from {} on {} within {} s; read it later with `sesync machine read`", sent.title, machine.name, limit.as_secs()))),
        }
    })
}

/// `read`: a conversation's last messages on that machine, oldest first.
fn read(path: &Path, name: &str, query: &str, count: &str) -> Result<(), Fail> {
    use super::prompt as p;
    let count: usize = count
        .parse()
        .ok()
        .filter(|n| (1..=p::MOST_MESSAGES).contains(n))
        .ok_or_else(|| Fail::Usage(format!("{count}: the count is a number from 1 to {}", p::MOST_MESSAGES)))?;
    let machine = named(path, name)?;
    let (title, messages) = runtime()?.block_on(p::read(&machine, query, count)).map_err(Fail::Failed)?;
    eprintln!("{title} on {}: last {} message(s)", machine.name, messages.len());
    for message in &messages {
        println!("{}\n", p::render(message));
    }
    Ok(())
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
        match &machine.local_endpoint {
            Some(local) => println!("{}  {local}  (dialled here instead of {})", machine.name, machine.endpoint),
            None => println!("{}  {}", machine.name, machine.endpoint),
        }
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
