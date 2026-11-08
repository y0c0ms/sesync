//! Confinement for the read-only file and git methods. A paired phone names paths relative to a
//! conversation's project directory. Every such path is checked lexically (no absolute path, no `..`,
//! no NUL, no `.git` entry), then resolved through symlinks and checked again against the canonical
//! project root. Nothing outside the root is read, and `.git` is never served: its config can hold
//! remote credentials. docs/BRIDGE.md, "Security", states the rule for the phone.

use super::rpc::Failure;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// The most text `files.read` returns. A larger file is cut to this many bytes and marked truncated.
pub const TEXT_CAP: u64 = 1024 * 1024;
/// The largest image `files.readPreview` returns. A larger image is refused with `file_too_large`.
pub const IMAGE_CAP: u64 = 2 * 1024 * 1024;

/// The git directory's name, never served at any depth.
pub const GIT_DIR: &str = ".git";

/// The refusal for a path that is malformed, leaves the project, or names `.git`.
pub fn invalid() -> Failure {
    ("invalid_relative_path", "invalid_relative_path".into())
}

/// The canonical project directory. Fails when the conversation records none, or the directory is gone.
pub fn root_of(dir: &str) -> Result<PathBuf, Failure> {
    if dir.is_empty() {
        return Err(("bad_request", "the conversation records no project directory".into()));
    }
    let root = std::fs::canonicalize(dir).map_err(|_| ("not_found", "project directory not found".into()))?;
    if !root.is_dir() {
        return Err(("not_a_directory", "the project directory is not a directory".into()));
    }
    Ok(root)
}

/// The components a relative path names, after the lexical checks. `""` and `"."` name the root and
/// give an empty path. Rejects absolute and prefixed paths, `..`, NUL bytes and any `.git` component.
pub fn lexical(relative: &str) -> Result<PathBuf, Failure> {
    if relative.contains('\0') {
        return Err(invalid());
    }
    let mut clean = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(name) if name == OsStr::new(GIT_DIR) => return Err(invalid()),
            Component::Normal(name) => clean.push(name),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return Err(invalid()),
        }
    }
    Ok(clean)
}

/// The path below `root` when the canonical `path` lies under it and names no `.git` component.
/// `path` must already be canonical; `None` means the path is outside the project.
pub fn relative_inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    if rel.components().any(|c| c.as_os_str() == OsStr::new(GIT_DIR)) {
        return None;
    }
    Some(rel.to_path_buf())
}

/// The canonical path of `root/clean`, which must exist and lie inside `root` after symlinks are
/// followed. A missing entry is `not_found`; one that leaves the project is `invalid_relative_path`.
pub fn existing(root: &Path, clean: &Path) -> Result<PathBuf, Failure> {
    let canonical = std::fs::canonicalize(root.join(clean)).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ("not_found", "file not found".into()),
        _ => ("unavailable", "the file cannot be read".into()),
    })?;
    relative_inside(root, &canonical).ok_or_else(invalid)?;
    Ok(canonical)
}

/// `relative` resolved under `root`: the canonical path of an existing entry inside the project.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf, Failure> {
    existing(root, &lexical(relative)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sesync-confine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project/src")).unwrap();
        std::fs::write(dir.join("project/src/lib.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("secret.txt"), "outside\n").unwrap();
        std::fs::create_dir_all(dir.join("project/.git")).unwrap();
        std::fs::write(dir.join("project/.git/config"), "[remote]\n").unwrap();
        dir
    }

    fn code(result: Result<PathBuf, Failure>) -> &'static str {
        result.unwrap_err().0
    }

    #[test]
    fn a_relative_path_inside_the_project_resolves() {
        let dir = sandbox("inside");
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(resolve(&root, "src/lib.rs").unwrap(), root.join("src/lib.rs"));
        assert_eq!(resolve(&root, "").unwrap(), root);
        assert_eq!(resolve(&root, "./src").unwrap(), root.join("src"));
    }

    #[test]
    fn dot_dot_and_absolute_paths_are_refused() {
        let dir = sandbox("dotdot");
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        for bad in ["../secret.txt", "src/../../secret.txt", "/etc/passwd", "src/../src/lib.rs"] {
            assert_eq!(code(resolve(&root, bad)), "invalid_relative_path", "{bad}");
        }
    }

    #[test]
    fn a_nul_byte_is_refused() {
        let dir = sandbox("nul");
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(code(resolve(&root, "src/lib.rs\0")), "invalid_relative_path");
    }

    #[test]
    fn git_internals_are_never_served() {
        let dir = sandbox("git");
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(code(resolve(&root, ".git/config")), "invalid_relative_path");
        assert_eq!(code(resolve(&root, ".git")), "invalid_relative_path");
    }

    #[test]
    fn a_symlink_that_leaves_the_root_is_refused() {
        let dir = sandbox("link-out");
        std::os::unix::fs::symlink(dir.join("secret.txt"), dir.join("project/leak.txt")).unwrap();
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(code(resolve(&root, "leak.txt")), "invalid_relative_path");
    }

    #[test]
    fn a_symlink_into_git_internals_is_refused() {
        let dir = sandbox("link-git");
        std::os::unix::fs::symlink(dir.join("project/.git/config"), dir.join("project/cfg")).unwrap();
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(code(resolve(&root, "cfg")), "invalid_relative_path");
    }

    #[test]
    fn a_symlink_that_stays_inside_the_root_resolves() {
        let dir = sandbox("link-in");
        std::os::unix::fs::symlink(dir.join("project/src/lib.rs"), dir.join("project/alias.rs")).unwrap();
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(resolve(&root, "alias.rs").unwrap(), root.join("src/lib.rs"));
    }

    #[test]
    fn a_missing_entry_is_not_found() {
        let dir = sandbox("missing");
        let root = root_of(dir.join("project").to_str().unwrap()).unwrap();
        assert_eq!(code(resolve(&root, "nope.txt")), "not_found");
    }

    #[test]
    fn an_empty_or_missing_project_directory_is_refused() {
        assert_eq!(root_of("").unwrap_err().0, "bad_request");
        assert_eq!(root_of("/nonexistent/sesync-confine-root").unwrap_err().0, "not_found");
    }
}
