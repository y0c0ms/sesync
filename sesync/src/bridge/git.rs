//! Read-only source control: the status, a file's diff, the commit history, a commit's files and a
//! branch's changes. Git runs only through a fixed allowlist of read-only verbs (see `run`), inside the
//! conversation's project directory.

use super::confine;
use super::rpc::{self, Ctx, Failure};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The most stdout one git call may produce. Past it the call fails rather than buffering without bound.
const OUTPUT_CAP: u64 = 8 * 1024 * 1024;
/// The stderr kept for a failure. Only its first line reaches the phone.
const STDERR_CAP: u64 = 64 * 1024;
/// How long one git call may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(5);
/// The commits `git.history` returns when the phone names no limit, and the most it ever returns.
const HISTORY_DEFAULT: u64 = 50;
const HISTORY_MAX: u64 = 200;
/// The byte separators of the `log --format` records `git.history` reads (`%x1f`, `%x1e`).
const FIELD_SEP: u8 = 0x1f;
const RECORD_SEP: u8 = 0x1e;

/// `git.status {worktree, includeLineStats?, …}`: changed entries, branch and upstream.
pub fn status(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let listing = run(&root, &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"])?;
    let porcelain = parse_porcelain(&listing)?;
    let (staged, unstaged) = if params["includeLineStats"].as_bool().unwrap_or(true) {
        let staged = run(
            &root,
            &["diff", "--cached", "--no-ext-diff", "--no-textconv", "--no-color", "--numstat", "-z", "-M", "--"],
        )?;
        let unstaged = run(&root, &["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--numstat", "-z", "--"])?;
        (stats(&staged)?, stats(&unstaged)?)
    } else {
        (HashMap::new(), HashMap::new())
    };

    let mut entries = Vec::with_capacity(porcelain.rows.len());
    for row in &porcelain.rows {
        let mut entry = Map::new();
        entry.insert("path".into(), json!(row.path));
        entry.insert("status".into(), json!(row.status));
        entry.insert("area".into(), json!(row.area));
        if let Some(old) = &row.old_path {
            entry.insert("oldPath".into(), json!(old));
        }
        if row.unresolved {
            entry.insert("conflictStatus".into(), json!("unresolved"));
            entry.insert("conflictStatusSource".into(), json!("git"));
        }
        let counts = match row.area {
            "staged" => staged.get(&row.path),
            "unstaged" => unstaged.get(&row.path),
            _ => None,
        };
        put_counts(&mut entry, counts.copied().flatten());
        entries.push(Value::Object(entry));
    }

    let mut out = Map::new();
    out.insert("entries".into(), Value::Array(entries));
    if let Some(branch) = porcelain.branch {
        out.insert("branch".into(), json!(branch));
    }
    if let Some(head) = porcelain.head {
        out.insert("head".into(), json!(head));
    }
    if let Some(name) = porcelain.upstream {
        let (ahead, behind) = porcelain.ahead_behind;
        out.insert(
            "upstreamStatus".into(),
            json!({"hasUpstream": true, "ahead": ahead, "behind": behind, "upstreamName": name}),
        );
    }
    Ok(Value::Object(out))
}

/// `git.diff {worktree, filePath, staged, compareAgainstHead?}`: one file's original and modified text.
pub fn diff(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let relative = params["filePath"].as_str().ok_or_else(|| bad("missing filePath"))?;
    let path = git_path(&confine::lexical(relative)?)?;
    let staged = params["staged"].as_bool().unwrap_or(false);
    // Unstaged changes are read against the index, unless the phone asks for HEAD.
    let original_rev = if staged || params["compareAgainstHead"].as_bool().unwrap_or(false) {
        format!("HEAD:{path}")
    } else {
        format!(":{path}")
    };
    let original = read_blob(&root, &original_rev)?;
    let modified = if staged { read_blob(&root, &format!(":{path}"))? } else { read_worktree(&root, relative)? };
    // A side over the cap is reported by size, which is what the phone's review screen renders.
    let largest = [&original, &modified]
        .into_iter()
        .filter_map(|side| match side {
            Blob::Large(size) => Some(*size),
            _ => None,
        })
        .max();
    if let Some(byte_length) = largest {
        return Ok(json!({"kind": "too-large", "byteLength": byte_length}));
    }
    match (text_side(&original.into_bytes()), text_side(&modified.into_bytes())) {
        (Side::Text(original), Side::Text(modified)) => {
            Ok(json!({"kind": "text", "originalContent": original, "modifiedContent": modified}))
        }
        _ => Ok(json!({"kind": "binary"})),
    }
}

/// `git.history {worktree, limit?, baseRef?}`: the newest commits, first.
pub fn history(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let limit = params["limit"].as_u64().unwrap_or(HISTORY_DEFAULT).clamp(1, HISTORY_MAX);
    let base = optional_ref(params, "baseRef")?;
    // An unborn HEAD has no history yet, which is an empty list rather than a failure.
    if commit_oid(&root, "HEAD")?.is_none() {
        return Ok(json!({"items": []}));
    }
    let range = match base {
        Some(base) => {
            if commit_oid(&root, base)?.is_none() {
                return Err(bad("baseRef does not name a commit"));
            }
            format!("{base}..HEAD")
        }
        None => "HEAD".to_string(),
    };
    let count = format!("-n{limit}");
    let raw = run(
        &root,
        &[
            "log",
            count.as_str(),
            "--no-color",
            "--no-show-signature",
            "--format=%H%x1f%P%x1f%aN%x1f%at%x1f%s%x1e",
            range.as_str(),
            "--",
        ],
    )?;
    let items = parse_log(&raw)?;
    Ok(json!({"items": items}))
}

/// `git.commitCompare {worktree, commitId}`: the files one commit changed.
pub fn commit_compare(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let commit = commit_id(params)?;
    let names = run(
        &root,
        &["show", "--no-ext-diff", "--no-textconv", "--no-color", "--no-show-signature", "--first-parent", "-M", "-z", "--name-status", "--format=", commit],
    )?;
    let counts = run(
        &root,
        &["show", "--no-ext-diff", "--no-textconv", "--no-color", "--no-show-signature", "--first-parent", "-M", "-z", "--numstat", "--format=", commit],
    )?;
    let entries = changed_entries(&names, &counts)?;
    Ok(json!({"entries": entries}))
}

/// `git.branchCompare {worktree, baseRef}`: the files HEAD changed since it forked from `baseRef`.
pub fn branch_compare(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let base = required_ref(params, "baseRef")?;
    let compare = compare_branch(&root, base)?;
    // The checked-out branch's name, which is `HEAD` when detached or when there is no commit yet.
    let mut compare_ref = "HEAD".to_string();
    if compare.head_oid.is_some() {
        let name = first_line(&run(&root, &["rev-parse", "--abbrev-ref", "HEAD"])?);
        if !name.is_empty() {
            compare_ref = name;
        }
    }
    let mut summary = json!({
        "baseRef": base,
        "compareRef": compare_ref,
        "changedFiles": compare.entries.len(),
        "status": compare.status,
        "baseOid": compare.base_oid,
        "headOid": compare.head_oid,
        "mergeBase": compare.merge_base,
        "commitsAhead": compare.commits_ahead,
    });
    if let Some(message) = compare.error {
        summary["errorMessage"] = json!(message);
    }
    Ok(json!({"summary": summary, "entries": compare.entries}))
}

/// Runs one allowlisted, read-only git call in `root` and returns its stdout. This is the only place git
/// is spawned: `allowed` vets every verb and option first, and the fixed `-c` flags switch off the
/// fsmonitor and hooks, so a repository's own configuration cannot run code or write.
fn run(root: &Path, argv: &[&str]) -> Result<Vec<u8>, Failure> {
    allowed(argv)?;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=", "-c", "core.hooksPath=/dev/null"])
        .args(argv)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        // A repository named by the environment would override `-C`.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => unavailable("git is not installed".into()),
            _ => unavailable(format!("cannot run git: {e}")),
        })?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        kill_child(&mut child);
        return Err(unavailable("cannot read git output".into()));
    };
    let out_reader = thread::spawn(move || capped(stdout, OUTPUT_CAP));
    let err_reader = thread::spawn(move || capped(stderr, STDERR_CAP));
    let status = match wait_for(&mut child) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill_child(&mut child);
            return Err(unavailable(format!("git timed out after {} seconds", TIMEOUT.as_secs())));
        }
        Err(failure) => {
            kill_child(&mut child);
            return Err(failure);
        }
    };
    let (stdout, overflowed) = join(out_reader)?;
    let (stderr, _) = join(err_reader)?;
    if !status.success() {
        return Err(git_failure(&stderr));
    }
    if overflowed {
        return Err(too_large());
    }
    Ok(stdout)
}

/// Refuses every argv that is not one of the read-only verbs with an option it lists. A dash-led
/// revision or path would be read by git as an option, so only the `--`-separated tail may hold one,
/// and there it is refused too.
fn allowed(argv: &[&str]) -> Result<(), Failure> {
    let Some((verb, args)) = argv.split_first() else {
        return Err(refused());
    };
    let options: &[&str] = match *verb {
        "status" => &["--porcelain=v2", "-z", "--branch", "--untracked-files=all"],
        "diff" => &[
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--cached",
            "--numstat",
            "--name-status",
            "-z",
            "-M",
        ],
        "log" => &["--no-color", "--no-show-signature"],
        "show" => &[
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-show-signature",
            "--format=",
            "--name-status",
            "--numstat",
            "-z",
            "-M",
            "--first-parent",
        ],
        "merge-base" => &[],
        "rev-parse" => &["--verify", "--quiet", "--abbrev-ref"],
        _ => return Err(refused()),
    };
    let takes_separator = matches!(*verb, "diff" | "log" | "show");
    let mut in_paths = false;
    for arg in args {
        if in_paths {
            if arg.starts_with('-') {
                return Err(refused());
            }
        } else if *arg == "--" {
            if !takes_separator {
                return Err(refused());
            }
            in_paths = true;
        } else if arg.starts_with('-') && !options.contains(arg) && !valued_option(verb, arg) {
            return Err(refused());
        }
    }
    Ok(())
}

/// Options that carry a value: a `--format=` layout for `log` and `show`, and `-n<count>` for `log`.
fn valued_option(verb: &str, arg: &str) -> bool {
    match verb {
        "log" => {
            arg.starts_with("--format=")
                || arg
                    .strip_prefix("-n")
                    .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
        }
        "show" => arg.starts_with("--format="),
        _ => false,
    }
}

/// The phone's failure for a git call that exited non-zero. A directory git does not know is named as such.
fn git_failure(stderr: &[u8]) -> Failure {
    let text = String::from_utf8_lossy(stderr);
    if text.contains("not a git repository") {
        return ("not_a_repository", "not a git repository".into());
    }
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("git failed");
    ("git_failed", line.chars().take(200).collect())
}

/// Reads a pipe to its end, keeping at most `cap` bytes. The rest is drained, not dropped, so git never
/// blocks writing into a pipe nobody reads; the flag reports that the cap was exceeded.
fn capped(mut pipe: impl Read, cap: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    (&mut pipe).take(cap + 1).read_to_end(&mut kept)?;
    let overflowed = kept.len() as u64 > cap;
    io::copy(&mut pipe, &mut io::sink())?;
    Ok((kept, overflowed))
}

fn join(reader: JoinHandle<io::Result<(Vec<u8>, bool)>>) -> Result<(Vec<u8>, bool), Failure> {
    reader
        .join()
        .map_err(|_| unavailable("git output reader panicked".into()))?
        .map_err(|e| unavailable(format!("cannot read git output: {e}")))
}

/// Waits for `child` until the deadline; `None` means it was still running then.
fn wait_for(child: &mut Child) -> Result<Option<ExitStatus>, Failure> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| unavailable(format!("cannot wait for git: {e}")))? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The object a revision names, or `None` when it names nothing. A quiet `rev-parse` fails silently for
/// an absent name, so only that failure reads as absence; a missing repository or git still fails.
fn rev_oid(root: &Path, rev: &str) -> Result<Option<String>, Failure> {
    match run(root, &["rev-parse", "--verify", "--quiet", rev]) {
        Ok(out) => {
            let oid = first_line(&out);
            Ok((!oid.is_empty()).then_some(oid))
        }
        Err((code, _)) if code == "git_failed" => Ok(None),
        Err(failure) => Err(failure),
    }
}

fn commit_oid(root: &Path, rev: &str) -> Result<Option<String>, Failure> {
    rev_oid(root, &format!("{rev}^{{commit}}"))
}

fn first_line(out: &[u8]) -> String {
    String::from_utf8_lossy(out).lines().next().unwrap_or("").trim().to_string()
}

/// One side of a diff before it is shaped for the phone: absent (a new or deleted file's empty side), its
/// bytes, or its size when the file is over the cap and is never read in full.
enum Blob {
    Absent,
    Bytes(Vec<u8>),
    Large(u64),
}

impl Blob {
    /// The bytes of a side that was read. An absent or oversized side has none.
    fn into_bytes(self) -> Vec<u8> {
        match self {
            Blob::Bytes(bytes) => bytes,
            Blob::Absent | Blob::Large(_) => Vec::new(),
        }
    }
}

/// The content of the blob at `HEAD:path` or `:path`, or `Absent` when the path is not there.
fn read_blob(root: &Path, rev: &str) -> Result<Blob, Failure> {
    match rev_oid(root, rev)? {
        Some(oid) => {
            let bytes = run(root, &["show", "--no-ext-diff", "--no-textconv", "--no-color", "--no-show-signature", oid.as_str()])?;
            Ok(sized(bytes))
        }
        None => Ok(Blob::Absent),
    }
}

/// `bytes` as a side, or its size when it is over the text cap.
fn sized(bytes: Vec<u8>) -> Blob {
    let len = bytes.len() as u64;
    if len > confine::TEXT_CAP {
        Blob::Large(len)
    } else {
        Blob::Bytes(bytes)
    }
}

/// The working-tree content of a confined file: absent when it was deleted, its size when it is over the cap.
fn read_worktree(root: &Path, relative: &str) -> Result<Blob, Failure> {
    let path = match confine::resolve(root, relative) {
        Ok(path) => path,
        Err((code, _)) if code == "not_found" => return Ok(Blob::Absent),
        Err(failure) => return Err(failure),
    };
    let unreadable = |e: io::Error| unavailable(format!("cannot read {relative}: {e}"));
    let meta = fs::metadata(&path).map_err(unreadable)?;
    if !meta.is_file() {
        return Err(("not_a_file", "not_a_file".into()));
    }
    if meta.len() > confine::TEXT_CAP {
        return Ok(Blob::Large(meta.len()));
    }
    fs::read(&path).map(Blob::Bytes).map_err(unreadable)
}

/// One side of a diff: text, or binary, which the phone shows without a body.
enum Side {
    Text(String),
    Binary,
}

fn text_side(bytes: &[u8]) -> Side {
    if bytes.contains(&0) {
        return Side::Binary;
    }
    std::str::from_utf8(bytes).map_or(Side::Binary, |text| Side::Text(text.to_string()))
}

/// The repository path git is given for a confined relative path. An empty path names the root, not a file.
fn git_path(clean: &Path) -> Result<String, Failure> {
    match clean.to_str() {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => Err(confine::invalid()),
    }
}

/// A `log` record stream as history items, newest first. A subject-less commit gets the desktop's placeholder.
fn parse_log(raw: &[u8]) -> Result<Vec<Value>, Failure> {
    let mut items = Vec::new();
    for record in raw.split(|b| *b == RECORD_SEP) {
        let text = String::from_utf8_lossy(record);
        let text = text.trim_start_matches('\n');
        if text.is_empty() {
            continue;
        }
        let fields: Vec<&str> = text.splitn(5, char::from(FIELD_SEP)).collect();
        let (id, parents, author, at, subject) = match fields[..] {
            [id, parents, author, at, subject] => (id, parents, author, at, subject),
            _ => return Err(malformed()),
        };
        let subject = subject.trim();
        let mut item = Map::new();
        item.insert("id".into(), json!(id));
        item.insert("parentIds".into(), json!(parents.split_whitespace().collect::<Vec<_>>()));
        item.insert(
            "subject".into(),
            json!(if subject.is_empty() { "(no commit message)" } else { subject }),
        );
        item.insert("displayId".into(), json!(id.get(..7).unwrap_or(id)));
        if !author.is_empty() {
            item.insert("author".into(), json!(author));
        }
        // Milliseconds, as the desktop history reports them.
        item.insert("timestamp".into(), json!(at.parse::<i64>().ok().and_then(|s| s.checked_mul(1000))));
        items.push(Value::Object(item));
    }
    Ok(items)
}

/// What `git.status` reports for the working tree, parsed from `status --porcelain=v2 -z --branch`.
#[derive(Default)]
struct Porcelain {
    rows: Vec<Row>,
    branch: Option<String>,
    head: Option<String>,
    upstream: Option<String>,
    ahead_behind: (u64, u64),
}

/// One row of the status: a path in one area, with its rename origin and conflict marker.
struct Row {
    path: String,
    status: &'static str,
    area: &'static str,
    old_path: Option<String>,
    unresolved: bool,
}

fn parse_porcelain(raw: &[u8]) -> Result<Porcelain, Failure> {
    let mut porcelain = Porcelain::default();
    let mut fields = nul_fields(raw);
    while let Some(field) = fields.next() {
        let record = field.as_str();
        if record.is_empty() {
            continue;
        }
        if let Some(oid) = record.strip_prefix("# branch.oid ") {
            // An unborn branch reports `(initial)` where the commit would be.
            if oid != "(initial)" {
                porcelain.head = Some(oid.to_string());
            }
        } else if let Some(name) = record.strip_prefix("# branch.head ") {
            if name != "(detached)" {
                porcelain.branch = Some(name.to_string());
            }
        } else if let Some(name) = record.strip_prefix("# branch.upstream ") {
            porcelain.upstream = Some(name.to_string());
        } else if let Some(counts) = record.strip_prefix("# branch.ab ") {
            porcelain.ahead_behind = ahead_behind(counts)?;
        } else if let Some(rest) = record.strip_prefix("1 ") {
            let f = record_fields(rest, 8)?;
            push_tracked(&mut porcelain.rows, f[0], f[7], None)?;
        } else if let Some(rest) = record.strip_prefix("2 ") {
            // A rename or copy: its origin path is the next NUL field.
            let f = record_fields(rest, 9)?;
            let origin = next_field(&mut fields)?;
            push_tracked(&mut porcelain.rows, f[0], f[8], Some(origin))?;
        } else if let Some(rest) = record.strip_prefix("u ") {
            let f = record_fields(rest, 10)?;
            porcelain.rows.push(Row {
                path: f[9].to_string(),
                status: "modified",
                area: "unstaged",
                old_path: None,
                unresolved: true,
            });
        } else if let Some(path) = record.strip_prefix("? ") {
            porcelain.rows.push(Row {
                path: path.to_string(),
                status: "untracked",
                area: "untracked",
                old_path: None,
                unresolved: false,
            });
        }
    }
    Ok(porcelain)
}

/// A tracked entry's rows: one staged row when X is set, one unstaged row when Y is set.
fn push_tracked(rows: &mut Vec<Row>, xy: &str, path: &str, origin: Option<String>) -> Result<(), Failure> {
    let &[x, y] = xy.as_bytes() else {
        return Err(malformed());
    };
    let origin_for = |letter: u8| if matches!(letter, b'R' | b'C') { origin.clone() } else { None };
    if x != b'.' {
        rows.push(Row {
            path: path.to_string(),
            status: status_word(x),
            area: "staged",
            old_path: origin_for(x),
            unresolved: false,
        });
    }
    if y != b'.' {
        rows.push(Row {
            path: path.to_string(),
            status: status_word(y),
            area: "unstaged",
            old_path: origin_for(y),
            unresolved: false,
        });
    }
    Ok(())
}

fn ahead_behind(counts: &str) -> Result<(u64, u64), Failure> {
    let mut parts = counts.split(' ');
    let ahead = parts.next().and_then(|a| a.strip_prefix('+')).and_then(|a| a.parse::<u64>().ok());
    let behind = parts.next().and_then(|b| b.strip_prefix('-')).and_then(|b| b.parse::<u64>().ok());
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => Ok((ahead, behind)),
        _ => Err(malformed()),
    }
}

/// One changed path from `--name-status`. A rename or copy also carries the path it came from.
struct Change {
    path: String,
    status: &'static str,
    old_path: Option<String>,
}

fn name_status(raw: &[u8]) -> Result<Vec<Change>, Failure> {
    let mut fields = nul_fields(raw);
    let mut changes = Vec::new();
    while let Some(field) = fields.next() {
        let code = field.trim_start_matches('\n');
        if code.is_empty() {
            continue;
        }
        let status = code.bytes().next().map_or("modified", status_word);
        let origin = if code.starts_with(|c: char| c == 'R' || c == 'C') {
            Some(next_field(&mut fields)?)
        } else {
            None
        };
        let path = next_field(&mut fields)?;
        changes.push(Change { path, status, old_path: origin });
    }
    Ok(changes)
}

/// Line counts per path, from `--numstat -z`. A binary file has no counts, recorded as `None`. A rename's
/// record is an empty path field followed by its old and new paths; the counts are keyed by the new one.
fn stats(raw: &[u8]) -> Result<HashMap<String, Option<(u64, u64)>>, Failure> {
    let mut fields = nul_fields(raw);
    let mut counts = HashMap::new();
    while let Some(field) = fields.next() {
        let record = field.trim_start_matches('\n');
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(malformed());
        };
        let path = if path.is_empty() {
            next_field(&mut fields)?;
            next_field(&mut fields)?
        } else {
            path.to_string()
        };
        let pair = match (added.parse::<u64>(), removed.parse::<u64>()) {
            (Ok(added), Ok(removed)) => Some((added, removed)),
            _ => None,
        };
        counts.insert(path, pair);
    }
    Ok(counts)
}

/// Each `--name-status` change with its counts from the matching `--numstat` records.
fn changed_entries(names: &[u8], counts: &[u8]) -> Result<Vec<Value>, Failure> {
    let counts = stats(counts)?;
    name_status(names)?
        .into_iter()
        .map(|change| {
            let mut entry = Map::new();
            entry.insert("path".into(), json!(change.path));
            entry.insert("status".into(), json!(change.status));
            if let Some(old) = change.old_path {
                entry.insert("oldPath".into(), json!(old));
            }
            put_counts(&mut entry, counts.get(&change.path).copied().flatten());
            Ok(Value::Object(entry))
        })
        .collect()
}

/// What `git.branchCompare` found, before it is shaped for the phone.
#[derive(Default)]
struct Compare {
    status: &'static str,
    head_oid: Option<String>,
    base_oid: Option<String>,
    merge_base: Option<String>,
    commits_ahead: u64,
    entries: Vec<Value>,
    error: Option<String>,
}

fn compare_branch(root: &Path, base: &str) -> Result<Compare, Failure> {
    let mut compare = Compare::default();
    let Some(head) = commit_oid(root, "HEAD")? else {
        compare.status = "unborn-head";
        compare.error = Some("This branch has no commit yet, so compare-to-base is unavailable.".into());
        return Ok(compare);
    };
    compare.head_oid = Some(head);
    let Some(base_oid) = commit_oid(root, base)? else {
        compare.status = "invalid-base";
        compare.error = Some(format!("Base ref {base} could not be resolved in this repository."));
        return Ok(compare);
    };
    compare.base_oid = Some(base_oid.clone());
    let merge_base = match run(root, &["merge-base", base_oid.as_str(), "HEAD"]) {
        Ok(out) => first_line(&out),
        // `merge-base` exits with a plain failure when the two histories share no commit.
        Err((code, _)) if code == "git_failed" => {
            compare.status = "no-merge-base";
            compare.error = Some(format!(
                "This branch and {base} do not share a merge base, so compare-to-base is unavailable."
            ));
            return Ok(compare);
        }
        Err(failure) => return Err(failure),
    };
    compare.merge_base = Some(merge_base.clone());
    match changes_since(root, &merge_base) {
        Ok((entries, ahead)) => {
            compare.status = "ready";
            compare.entries = entries;
            compare.commits_ahead = ahead;
        }
        Err((_, message)) => {
            compare.status = "error";
            compare.error = Some(message);
        }
    }
    Ok(compare)
}

/// The files HEAD changed since `merge_base`, and how many commits HEAD is ahead of it.
fn changes_since(root: &Path, merge_base: &str) -> Result<(Vec<Value>, u64), Failure> {
    let range = format!("{merge_base}..HEAD");
    let names = run(
        root,
        &["diff", "--no-ext-diff", "--no-textconv", "--no-color", "-M", "-z", "--name-status", merge_base, "HEAD", "--"],
    )?;
    let counts = run(
        root,
        &["diff", "--no-ext-diff", "--no-textconv", "--no-color", "-M", "-z", "--numstat", merge_base, "HEAD", "--"],
    )?;
    let commits = run(
        root,
        &["log", "--no-color", "--no-show-signature", "--format=%H", range.as_str(), "--"],
    )?;
    let ahead = commits.split(|b| *b == b'\n').filter(|line| !line.is_empty()).count() as u64;
    Ok((changed_entries(&names, &counts)?, ahead))
}

/// The NUL-separated fields of `-z` output, as text.
fn nul_fields(raw: &[u8]) -> impl Iterator<Item = String> + '_ {
    raw.split(|b| *b == 0).map(|field| String::from_utf8_lossy(field).into_owned())
}

fn next_field(fields: &mut impl Iterator<Item = String>) -> Result<String, Failure> {
    fields.next().ok_or_else(malformed)
}

/// The fields of a porcelain record after its type letter, which must number exactly `count`.
fn record_fields(rest: &str, count: usize) -> Result<Vec<&str>, Failure> {
    let fields: Vec<&str> = rest.splitn(count, ' ').collect();
    if fields.len() == count {
        Ok(fields)
    } else {
        Err(malformed())
    }
}

fn put_counts(entry: &mut Map<String, Value>, counts: Option<(u64, u64)>) {
    if let Some((added, removed)) = counts {
        entry.insert("added".into(), json!(added));
        entry.insert("removed".into(), json!(removed));
    }
}

/// The phone's word for a git status letter.
fn status_word(letter: u8) -> &'static str {
    match letter {
        b'A' => "added",
        b'D' => "deleted",
        b'R' => "renamed",
        b'C' => "copied",
        _ => "modified",
    }
}

/// A ref the phone names, checked before git sees it: a branch, tag or remote name, never an option or a range.
fn ref_name<'a>(key: &str, value: &'a str) -> Result<&'a str, Failure> {
    let valid = (1..=255).contains(&value.len())
        && !value.starts_with('-')
        && !value.contains("..")
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/@+-".contains(&b));
    if valid {
        Ok(value)
    } else {
        Err(bad(format!("{key} is not a valid ref name")))
    }
}

fn required_ref<'a>(params: &'a Value, key: &str) -> Result<&'a str, Failure> {
    let value = params[key].as_str().ok_or_else(|| bad(format!("missing {key}")))?;
    ref_name(key, value)
}

fn optional_ref<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, Failure> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let text = value.as_str().ok_or_else(|| bad(format!("{key} must be a string")))?;
            ref_name(key, text).map(Some)
        }
    }
}

/// A full commit id: 40 hex digits, or 64 for SHA-256 repositories.
fn commit_id(params: &Value) -> Result<&str, Failure> {
    let id = params["commitId"].as_str().ok_or_else(|| bad("missing commitId"))?;
    let hex = id.bytes().all(|b| b.is_ascii_hexdigit());
    if hex && (id.len() == 40 || id.len() == 64) {
        Ok(id)
    } else {
        Err(bad("commitId must be a full commit id"))
    }
}

/// A `bad_request` for a malformed parameter. The message names the problem for the phone's log.
fn bad(message: impl Into<String>) -> Failure {
    ("bad_request", message.into())
}

fn refused() -> Failure {
    ("git_refused", "git_refused".into())
}

fn too_large() -> Failure {
    ("file_too_large", "file_too_large".into())
}

fn unavailable(message: String) -> Failure {
    ("unavailable", message)
}

/// Git printed output this module cannot read. Only a git this module was not built against does that.
fn malformed() -> Failure {
    ("git_failed", "unexpected git output".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::testkit::{conversation, ctx_with, sandbox};
    use std::path::PathBuf;
    use std::process::Command;

    /// Runs git in a fixture and returns its stdout. Fixtures may use git; the code under test may not.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(dir: &Path, name: &str, body: impl AsRef<[u8]>) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn repo(name: &str) -> PathBuf {
        let dir = sandbox(name);
        git(&dir, &["init", "-q", "-b", "main"]);
        dir
    }

    fn context(dir: &Path) -> Ctx {
        ctx_with(vec![conversation("work", dir.to_str().unwrap())], dir)
    }

    /// Request parameters naming the conversation at `dir`, plus `extra`.
    fn request(dir: &Path, extra: Value) -> Value {
        let mut value = json!({"worktree": conversation("work", dir.to_str().unwrap()).worktree_id()});
        for (key, field) in extra.as_object().unwrap() {
            value[key.as_str()] = field.clone();
        }
        value
    }

    /// A repository with a committed history, a feature branch, a conflicted merge, a modified file, a
    /// staged new file, a staged rename and an untracked file. Returns it and HEAD's commit id.
    fn fixture(name: &str) -> (PathBuf, String) {
        let dir = repo(name);
        write(&dir, "a.txt", "one\n");
        write(&dir, "b.txt", "two\n");
        write(&dir, "r.txt", "alpha\nbeta\ngamma\ndelta\nepsilon\n");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "initial"]);
        git(&dir, &["branch", "feature"]);
        git(&dir, &["checkout", "-q", "feature"]);
        write(&dir, "b.txt", "two-feature\n");
        git(&dir, &["commit", "-q", "-am", "feature b"]);
        git(&dir, &["checkout", "-q", "main"]);
        write(&dir, "b.txt", "two-main\n");
        git(&dir, &["commit", "-q", "-am", "main b"]);
        let merge = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "merge", "--no-edit", "feature"])
            .output()
            .unwrap();
        assert!(!merge.status.success(), "the merge must conflict");
        write(&dir, "a.txt", "one-edited\n");
        write(&dir, "s.txt", "new\n");
        git(&dir, &["add", "s.txt"]);
        git(&dir, &["mv", "r.txt", "r2.txt"]);
        write(&dir, "u.txt", "loose\n");
        let head = git(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        (dir, head)
    }

    fn assert_refused(argv: &[&str]) {
        assert_eq!(allowed(argv), Err(refused()), "{argv:?}");
    }

    #[test]
    fn allowed_passes_each_listed_verb_and_option() {
        let cases: &[&[&str]] = &[
            &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"],
            &[
                "diff", "--no-ext-diff", "--no-textconv", "--no-color", "--cached", "--numstat", "--name-status",
                "-z", "-M", "--", "a.txt",
            ],
            &["log", "--no-color", "--no-show-signature", "--format=%H", "-n50", "HEAD", "--"],
            &[
                "show", "--no-ext-diff", "--no-textconv", "--no-color", "--no-show-signature", "--format=", "--name-status", "--numstat",
                "-z", "-M", "--first-parent", "--", "a.txt",
            ],
            &["merge-base", "abc", "HEAD"],
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ];
        for argv in cases {
            assert_eq!(allowed(argv), Ok(()), "{argv:?}");
        }
    }

    #[test]
    fn allowed_refuses_every_other_verb() {
        for verb in [
            "commit", "add", "reset", "checkout", "switch", "restore", "stash", "push", "pull", "fetch", "merge",
            "rebase", "cherry-pick", "tag", "branch", "config", "update-ref", "clean", "rm", "mv", "apply", "am",
            "gc", "rev-list", "fsck", "-c", "",
        ] {
            assert_refused(&[verb]);
        }
        assert_refused(&[]);
    }

    #[test]
    fn allowed_refuses_unlisted_options() {
        let cases: &[&[&str]] = &[
            &["diff", "--output=x"],
            &["show", "--output=x"],
            &["log", "--output=x"],
            &["status", "-c", "core.pager=x"],
            &["diff", "--git-dir=x"],
            &["status", "--work-tree=x"],
            &["diff", "--ext-diff"],
            &["diff", "--textconv"],
            &["show", "--textconv"],
            &["diff", "--format=%H"],
            &["log", "-n"],
            &["log", "-n5x"],
            &["status", "--porcelain"],
            &["merge-base", "--is-ancestor", "a", "b"],
            &["rev-parse", "--end-of-options", "x"],
            &["status", "--"],
            &["merge-base", "--", "a"],
        ];
        for argv in cases {
            assert_refused(argv);
        }
    }

    #[test]
    fn allowed_refuses_dash_led_revisions_and_paths() {
        let cases: &[&[&str]] = &[
            &["rev-parse", "--verify", "-x"],
            &["merge-base", "-x", "HEAD"],
            &["diff", "-x"],
            &["log", "--", "-x"],
            &["show", "--", "-x"],
            &["show", "--format=%H", "-x"],
            &["diff", "--", "a", "--"],
            &["diff", "--", "--output=x"],
        ];
        for argv in cases {
            assert_refused(argv);
        }
    }

    #[test]
    fn revisions_and_ids_are_checked_before_git_sees_them() {
        for good in ["main", "feature/x", "v1.0", "release-2+x", "a@b", "@"] {
            assert!(ref_name("baseRef", good).is_ok(), "{good}");
        }
        for bad_name in ["", "-x", "--output=x", "a..b", "a b", "a:b", "x@{1}"] {
            assert!(ref_name("baseRef", bad_name).is_err(), "{bad_name}");
        }
        assert!(ref_name("baseRef", &"a".repeat(256)).is_err());
        assert!(ref_name("baseRef", &"a".repeat(255)).is_ok());

        let sha1 = "a".repeat(40);
        let sha256 = "0".repeat(64);
        assert!(commit_id(&json!({"commitId": sha1})).is_ok());
        assert!(commit_id(&json!({"commitId": sha256})).is_ok());
        assert!(commit_id(&json!({"commitId": "a".repeat(39)})).is_err());
        assert!(commit_id(&json!({"commitId": format!("{}g", "a".repeat(39))})).is_err());
        assert!(commit_id(&json!({"commitId": "-x"})).is_err());
    }

    #[test]
    fn write_verbs_are_refused_and_leave_the_repository_unchanged() {
        let (dir, head) = fixture("refused");
        let status_before = git(&dir, &["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
        let cases: [&[&str]; 6] = [
            &["checkout", "-b", "x"],
            &["commit", "-m", "x"],
            &["reset", "--hard"],
            &["stash"],
            &["push"],
            &["clean", "-fd"],
        ];
        for argv in cases {
            assert_eq!(run(&dir, argv), Err(refused()), "{argv:?}");
        }
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]).trim(), head);
        assert_eq!(git(&dir, &["status", "--porcelain=v2", "-z", "--untracked-files=all"]), status_before);
        assert!(dir.join("u.txt").exists());
    }

    #[test]
    fn status_reports_every_kind_of_change() {
        let (dir, head) = fixture("status");
        let value = status(&request(&dir, json!({})), &context(&dir)).unwrap();
        let entries = value["entries"].as_array().unwrap();
        let find = |path: &str, area: &str| {
            entries
                .iter()
                .find(|e| e["path"] == path && e["area"] == area)
                .unwrap_or_else(|| panic!("no {area} row for {path}: {value}"))
                .clone()
        };

        let edited = find("a.txt", "unstaged");
        assert_eq!(edited["status"], "modified");
        assert_eq!(edited["added"], 1);
        assert_eq!(edited["removed"], 1);

        let added = find("s.txt", "staged");
        assert_eq!(added["status"], "added");
        assert_eq!(added["added"], 1);
        assert_eq!(added["removed"], 0);

        let renamed = find("r2.txt", "staged");
        assert_eq!(renamed["status"], "renamed");
        assert_eq!(renamed["oldPath"], "r.txt");
        assert_eq!(renamed["added"], 0);

        let conflicted = find("b.txt", "unstaged");
        assert_eq!(conflicted["status"], "modified");
        assert_eq!(conflicted["conflictStatus"], "unresolved");
        assert_eq!(conflicted["conflictStatusSource"], "git");

        let untracked = find("u.txt", "untracked");
        assert_eq!(untracked["status"], "untracked");
        assert!(untracked.get("added").is_none());

        assert_eq!(value["branch"], "main");
        assert_eq!(value["head"], head.as_str());
        assert!(value.get("upstreamStatus").is_none());
    }

    #[test]
    fn status_without_line_stats_omits_counts() {
        let (dir, _) = fixture("status-nostats");
        let value = status(&request(&dir, json!({"includeLineStats": false})), &context(&dir)).unwrap();
        let edited = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"] == "a.txt")
            .unwrap();
        assert!(edited.get("added").is_none());
    }

    #[test]
    fn status_of_an_unborn_branch_has_no_head_and_counts_staged_files() {
        let dir = repo("unborn-status");
        write(&dir, "x.txt", "one\ntwo\n");
        git(&dir, &["add", "x.txt"]);
        let value = status(&request(&dir, json!({})), &context(&dir)).unwrap();
        assert_eq!(value["branch"], "main");
        assert!(value.get("head").is_none());
        let staged = &value["entries"][0];
        assert_eq!(staged["status"], "added");
        assert_eq!(staged["added"], 2);
    }

    #[test]
    fn diff_reads_each_side_from_its_source() {
        let (dir, _) = fixture("diff");
        let context = context(&dir);

        let unstaged = diff(&request(&dir, json!({"filePath": "a.txt", "staged": false})), &context).unwrap();
        assert_eq!(unstaged, json!({"kind": "text", "originalContent": "one\n", "modifiedContent": "one-edited\n"}));

        let staged = diff(&request(&dir, json!({"filePath": "s.txt", "staged": true})), &context).unwrap();
        assert_eq!(staged, json!({"kind": "text", "originalContent": "", "modifiedContent": "new\n"}));

        let untracked = diff(&request(&dir, json!({"filePath": "u.txt", "staged": false})), &context).unwrap();
        assert_eq!(untracked, json!({"kind": "text", "originalContent": "", "modifiedContent": "loose\n"}));

        let renamed_away = diff(&request(&dir, json!({"filePath": "r.txt", "staged": true})), &context).unwrap();
        assert_eq!(
            renamed_away,
            json!({"kind": "text", "originalContent": "alpha\nbeta\ngamma\ndelta\nepsilon\n", "modifiedContent": ""})
        );

        let deleted = {
            fs::remove_file(dir.join("a.txt")).unwrap();
            diff(&request(&dir, json!({"filePath": "a.txt", "staged": false})), &context).unwrap()
        };
        assert_eq!(deleted, json!({"kind": "text", "originalContent": "one\n", "modifiedContent": ""}));
    }

    #[test]
    fn diff_reports_binary_files() {
        let (dir, _) = fixture("diff-binary");
        write(&dir, "bin.dat", [0u8, 1, 2, 3]);
        git(&dir, &["add", "bin.dat"]);
        let value = diff(&request(&dir, json!({"filePath": "bin.dat", "staged": true})), &context(&dir)).unwrap();
        assert_eq!(value, json!({"kind": "binary"}));
    }

    #[test]
    fn diff_reports_oversized_sides_by_size_and_refuses_directories_and_escapes() {
        let (dir, _) = fixture("diff-refused");
        let context = context(&dir);
        write(&dir, "big.txt", "x".repeat(confine::TEXT_CAP as usize + 1));
        let big = diff(&request(&dir, json!({"filePath": "big.txt", "staged": false})), &context).unwrap();
        assert_eq!(big, json!({"kind": "too-large", "byteLength": confine::TEXT_CAP + 1}));

        fs::create_dir_all(dir.join("sub")).unwrap();
        let folder = diff(&request(&dir, json!({"filePath": "sub", "staged": false})), &context);
        assert_eq!(folder.unwrap_err().0, "not_a_file");

        for escaping in ["../x", ".git/config", ""] {
            let refused = diff(&request(&dir, json!({"filePath": escaping, "staged": false})), &context);
            assert_eq!(refused.unwrap_err().0, "invalid_relative_path", "{escaping}");
        }
    }

    #[test]
    fn history_lists_head_first_and_respects_base_and_limit() {
        let (dir, head) = fixture("history");
        let context = context(&dir);

        let all = history(&request(&dir, json!({})), &context).unwrap();
        let items = all["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], head.as_str());
        assert_eq!(items[0]["subject"], "main b");
        assert_eq!(items[0]["displayId"], &head[..7]);
        assert_eq!(items[0]["author"], "t");
        assert_eq!(items[0]["parentIds"].as_array().unwrap().len(), 1);
        assert!(items[0]["timestamp"].as_i64().unwrap() > 1_000_000_000_000);
        assert_eq!(items[1]["subject"], "initial");
        assert!(items[1]["parentIds"].as_array().unwrap().is_empty());

        let ahead = history(&request(&dir, json!({"baseRef": "feature"})), &context).unwrap();
        assert_eq!(ahead["items"].as_array().unwrap().len(), 1);
        assert_eq!(ahead["items"][0]["subject"], "main b");

        let limited = history(&request(&dir, json!({"limit": 1})), &context).unwrap();
        assert_eq!(limited["items"].as_array().unwrap().len(), 1);

        assert_eq!(history(&request(&dir, json!({"baseRef": "-x"})), &context).unwrap_err().0, "bad_request");
        assert_eq!(history(&request(&dir, json!({"baseRef": "nope"})), &context).unwrap_err().0, "bad_request");
    }

    #[test]
    fn history_of_an_unborn_branch_is_empty() {
        let dir = repo("unborn-history");
        let value = history(&request(&dir, json!({})), &context(&dir)).unwrap();
        assert_eq!(value, json!({"items": []}));
    }

    #[test]
    fn commit_compare_lists_the_files_a_commit_changed() {
        let (dir, head) = fixture("commit");
        let context = context(&dir);

        let value = commit_compare(&request(&dir, json!({"commitId": head})), &context).unwrap();
        assert_eq!(
            value,
            json!({"entries": [{"path": "b.txt", "status": "modified", "added": 1, "removed": 1}]})
        );

        let root = git(&dir, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string();
        let initial = commit_compare(&request(&dir, json!({"commitId": root})), &context).unwrap();
        let mut paths: Vec<&str> = initial["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        paths.sort_unstable();
        assert_eq!(paths, ["a.txt", "b.txt", "r.txt"]);
        assert!(initial["entries"].as_array().unwrap().iter().all(|e| e["status"] == "added"));

        let not_hex = commit_compare(&request(&dir, json!({"commitId": "xyz"})), &context);
        assert_eq!(not_hex.unwrap_err().0, "bad_request");
    }

    #[test]
    fn branch_compare_reports_changes_since_the_merge_base() {
        let (dir, head) = fixture("branch");
        let context = context(&dir);
        let feature = git(&dir, &["rev-parse", "feature"]).trim().to_string();
        let initial = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_string();

        let value = branch_compare(&request(&dir, json!({"baseRef": "feature"})), &context).unwrap();
        let summary = &value["summary"];
        assert_eq!(summary["status"], "ready");
        let branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
        assert_eq!(summary["compareRef"], branch.as_str());
        assert_eq!(summary["changedFiles"], 1);
        assert_eq!(summary["commitsAhead"], 1);
        assert_eq!(summary["baseOid"], feature.as_str());
        assert_eq!(summary["headOid"], head.as_str());
        assert_eq!(summary["mergeBase"], initial.as_str());
        assert_eq!(
            value["entries"],
            json!([{"path": "b.txt", "status": "modified", "added": 1, "removed": 1}])
        );

        let missing = branch_compare(&request(&dir, json!({"baseRef": "nope"})), &context).unwrap();
        assert_eq!(missing["summary"]["status"], "invalid-base");
        assert!(missing["summary"]["baseOid"].is_null());
        assert!(missing["entries"].as_array().unwrap().is_empty());

        let dash = branch_compare(&request(&dir, json!({"baseRef": "-x"})), &context);
        assert_eq!(dash.unwrap_err().0, "bad_request");
    }

    #[test]
    fn branch_compare_reports_unborn_head_and_unrelated_histories() {
        let dir = repo("unborn-compare");
        let unborn = branch_compare(&request(&dir, json!({"baseRef": "main"})), &context(&dir)).unwrap();
        assert_eq!(unborn["summary"]["status"], "unborn-head");

        write(&dir, "a.txt", "one\n");
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "first"]);
        git(&dir, &["checkout", "-q", "--orphan", "other"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "unrelated"]);
        let unrelated = branch_compare(&request(&dir, json!({"baseRef": "main"})), &context(&dir)).unwrap();
        assert_eq!(unrelated["summary"]["status"], "no-merge-base");
        assert!(unrelated["summary"]["baseOid"].is_string());
        assert!(unrelated["summary"]["mergeBase"].is_null());
    }

    #[test]
    fn a_directory_outside_a_repository_is_not_a_repository() {
        let dir = sandbox("not-a-repo");
        let failure = status(&request(&dir, json!({})), &context(&dir)).unwrap_err();
        assert_eq!(failure.0, "not_a_repository");
    }
}
