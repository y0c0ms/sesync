//! Mirror install. Ported from daemon/sesync.py install_jsonl and
//! _install_jsonl_sesync_conv: the converter writes the mirror, and the file is
//! replaced atomically so a reader never sees a half-written mirror.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub enum InstallTarget {
    /// Pi journal store: `<agent_dir>/sessions/<bucket>/<file>.jsonl`.
    Pi { agent_dir: PathBuf },
    /// omp journal store: `<agent_dir>/sessions/<bucket>/<file>.jsonl`.
    Omp { agent_dir: PathBuf, home: PathBuf },
    /// Claude Code store: `<claude_dir>/projects/<bucket>/<uuid>.jsonl`.
    Claude { claude_dir: PathBuf },
}

/// Convert `src` (an omp/Pi journal) into the target store, with a deterministic
/// mirror id, and return the installed path. Any converter failure is an error,
/// and nothing is left in the store.
pub fn install_journal(
    sesync_conv: &Path,
    src: &Path,
    target: InstallTarget,
    cwd: &str,
    mirror_id: &str,
    title: &str,
) -> Result<PathBuf, String> {
    let _ = cwd;
    let scratch = std::env::temp_dir().join(format!("hs-install-scratch-{}-{}", std::process::id(), mirror_id));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;

    let (sub, verb, store, extra): (&str, &str, &Path, Vec<String>) = match &target {
        InstallTarget::Pi { agent_dir } => ("sessions", "to-pi", agent_dir, vec![]),
        InstallTarget::Omp { agent_dir, home } => (
            "sessions", "to-omp", agent_dir,
            vec!["--home".into(), home.display().to_string()],
        ),
        InstallTarget::Claude { claude_dir } => ("projects", "to-claude", claude_dir, vec![]),
    };
    let out_dir = scratch.join(sub);
    let mut cmd = Command::new(sesync_conv);
    cmd.arg(verb).arg(src).arg("--out-dir").arg(&out_dir).args(&extra);
    // The mirror id names the file; --title carries the conversation name
    // (to-claude writes it as the custom-title record Claude labels the session by).
    cmd.args(["--id", mirror_id, "--title", title]);
    let out = cmd.output().map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!("sesync-conv {verb} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    let produced = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let produced = PathBuf::from(produced.lines().last().unwrap_or("").trim());
    if !produced.is_file() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!("sesync-conv printed no output file: {}", produced.display()));
    }

    let name = produced.file_name().ok_or("no file name")?.to_owned();
    let bucket = produced.parent().and_then(Path::file_name).ok_or("no bucket")?.to_owned();
    let dest_dir = store.join(sub).join(&bucket);
    fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    // Remove an older mirror with the same id, whatever its timestamp prefix.
    if let Ok(rd) = fs::read_dir(&dest_dir) {
        for e in rd.flatten() {
            let n = e.file_name();
            if n.to_string_lossy().ends_with(&format!("{mirror_id}.jsonl")) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let dest = dest_dir.join(&name);
    let tmp = dest.with_extension("jsonl.tmp");
    fs::copy(&produced, &tmp).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&scratch);
    Ok(dest)
}

/// Install an omp/Pi journal as a Gemini CLI session. `sesync-conv to-gemini` writes the
/// session file under the project's own slug (the one Gemini registered, or the one it would
/// generate) and registers the project in `projects.json` with a `.project_root` marker, so
/// `gemini --list-sessions` finds the session. There is no delete-first step: one conversation
/// keeps one deterministic file name, and the write is atomic, so a refresh replaces it in
/// place. Returns the installed file, taken from the printed path.
pub fn install_gemini(
    sesync_conv: &Path,
    src: &Path,
    gemini_dir: &Path,
    mirror_id: &str,
) -> Result<PathBuf, String> {
    let out = Command::new(sesync_conv)
        .arg("to-gemini").arg(src)
        .arg("--gemini-dir").arg(gemini_dir)
        .args(["--id", mirror_id])
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !out.status.success() {
        return Err(format!("sesync-conv to-gemini failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let produced = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let produced = PathBuf::from(produced.lines().last().unwrap_or("").trim());
    if !produced.is_file() {
        return Err(format!("sesync-conv printed no output file: {}", produced.display()));
    }
    Ok(produced)
}

/// Install an omp/Pi journal as an O-penCode session. Ported from install_opencode:
/// `sesync-conv to-opencode` writes a bundle, then `opencode import` loads it. Any previous
/// import of the same mirror id is deleted first, because `opencode import` only upserts the
/// session row and conflict-ignores message ids: without the delete a refresh would keep the
/// old turns and pile the new ones on top. The import runs in the conversation's directory
/// (from `cwd`, else the directory recorded in the bundle) so the session lands in the right
/// project. `env_xdg` points the CLI's XDG_* variables at a sandbox; `None` inherits the real
/// environment. The import must print "Imported session" or it is not counted as installed.
pub fn install_opencode(
    opencode: &Path,
    sesync_conv: &Path,
    src: &Path,
    cwd: &str,
    mirror_id: &str,
    title: &str,
    env_xdg: Option<&Path>,
) -> Result<String, String> {
    let scratch = std::env::temp_dir().join(format!("hs-oc-install-{}-{mirror_id}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let bundle = scratch.join("bundle.json");

    let conv = Command::new(sesync_conv)
        .arg("to-opencode").arg(src)
        .arg("--out").arg(&bundle)
        .args(["--id", mirror_id, "--title", title])
        .output()
        .map_err(|e| format!("cannot run sesync-conv: {e}"))?;
    if !conv.status.success() {
        let _ = fs::remove_dir_all(&scratch);
        return Err(format!("sesync-conv to-opencode failed: {}", String::from_utf8_lossy(&conv.stderr).trim()));
    }

    // Where the import runs decides the project and directory OpenCode stores the session
    // under. Prefer the conversation's own directory when it still exists, then the bundle's.
    let bundle_dir = fs::read_to_string(&bundle).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["info"]["directory"].as_str().map(str::to_string))
        .unwrap_or_default();
    let dir = if Path::new(cwd).is_dir() { cwd.to_string() } else { bundle_dir };
    let workdir = if Path::new(&dir).is_dir() { Some(PathBuf::from(dir)) } else { None };

    // Delete the previous import of this mirror. A missing session is the normal first
    // write and is ignored; any other delete failure surfaces on the import below.
    let mut del = Command::new(opencode);
    del.args(["session", "delete", mirror_id]);
    if let Some(w) = &workdir {
        del.current_dir(w);
    }
    if let Some(x) = env_xdg {
        del.env("XDG_DATA_HOME", x.join("data"))
            .env("XDG_CONFIG_HOME", x.join("cfg"))
            .env("XDG_STATE_HOME", x.join("state"))
            .env("XDG_CACHE_HOME", x.join("cache"));
    }
    let _ = del.output();

    let mut cmd = Command::new(opencode);
    cmd.arg("import").arg(&bundle);
    if let Some(w) = &workdir {
        cmd.current_dir(w);
    }
    if let Some(x) = env_xdg {
        cmd.env("XDG_DATA_HOME", x.join("data"))
            .env("XDG_CONFIG_HOME", x.join("cfg"))
            .env("XDG_STATE_HOME", x.join("state"))
            .env("XDG_CACHE_HOME", x.join("cache"));
    }
    let imp = cmd.output().map_err(|e| format!("cannot run opencode import: {e}"))?;
    let _ = fs::remove_dir_all(&scratch);
    let stdout = String::from_utf8_lossy(&imp.stdout);
    if !imp.status.success() || !stdout.contains("Imported session") {
        return Err(format!("opencode import failed: {}", String::from_utf8_lossy(&imp.stderr).trim()));
    }
    Ok(mirror_id.to_string())
}
