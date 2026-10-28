//! Read-only project files: the explorer tree, text and image previews, terminal path links and the
//! `@` file search. Every path is confined to the conversation's project directory (confine.rs).

use super::confine::{self, IMAGE_CAP, TEXT_CAP};
use super::rpc::{self, Ctx, Failure};
use super::uploads;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most entries `files.readDir` lists. A larger directory is refused, not paged: the phone's
/// tree would otherwise load an unbounded list at once.
const MAX_DIR_ENTRIES: usize = 5000;
/// The entries `files.searchPaths` walks before it stops and reports the walk truncated, so a huge
/// tree cannot hold the request.
const WALK_BUDGET: usize = 50_000;
/// The rows `files.searchPaths` returns when the request names no limit, and the most it ever returns.
const SEARCH_LIMIT_DEFAULT: i64 = 16;
const SEARCH_LIMIT_MAX: i64 = 200;
/// The leading bytes sniffed for a NUL, which marks a file binary whatever its extension.
const NUL_SNIFF_BYTES: usize = 8192;

/// Extensions the phone never reads as text. Lowercase, without the dot.
const BINARY_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "avif", "heic", "heif", "tif", "tiff", "psd",
    // audio and video
    "mp3", "wav", "flac", "ogg", "oga", "m4a", "aac", "opus", "wma", "aiff", "mp4", "mov", "avi", "mkv",
    "webm", "m4v", "wmv", "flv",
    // archives and packages
    "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "jar", "apk", "deb", "rpm", "iso", "dmg",
    "whl", "gem",
    // documents
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "pages", "numbers", "key",
    // fonts
    "ttf", "otf", "woff", "woff2", "eot",
    // executables, object and shared libraries, compiled output, databases
    "exe", "dll", "bin", "com", "msi", "o", "obj", "a", "lib", "so", "dylib", "ko", "class", "pyc", "wasm",
    "sqlite", "sqlite3",
];

/// `files.readDir {worktree, relativePath?}`: one directory's entries, as a bare array. The
/// `followSymlinks` option is not read: a symlink is always resolved and confined, never listed as
/// reaching outside the project.
pub fn read_dir(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let clean = confine::lexical(relative_param(params))?;
    let dir = if clean.as_os_str().is_empty() { root.clone() } else { confine::existing(&root, &clean)? };
    if !dir.is_dir() {
        return Err(refuse("not_a_directory"));
    }
    let listing = std::fs::read_dir(&dir).map_err(|_| unreadable())?;
    let mut entries = Vec::new();
    for (index, entry) in listing.enumerate() {
        if index == MAX_DIR_ENTRIES {
            return Err(refuse("directory_too_large"));
        }
        // An entry that vanished while the directory was read has nothing to show.
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == confine::GIT_DIR {
            continue;
        }
        let Ok(file_type) = entry.file_type() else { continue };
        let (is_directory, is_symlink) = if file_type.is_symlink() {
            let Ok(target) = std::fs::canonicalize(entry.path()) else { continue };
            if confine::relative_inside(&root, &target).is_none() {
                continue;
            }
            (target.is_dir(), true)
        } else {
            (file_type.is_dir(), false)
        };
        entries.push(Listed { name, is_directory, is_symlink });
    }
    entries.sort_by_cached_key(|e| (!e.is_directory, e.name.to_lowercase()));
    let rows = entries
        .iter()
        .map(|e| json!({"name": e.name, "isDirectory": e.is_directory, "isSymlink": e.is_symlink}))
        .collect();
    Ok(Value::Array(rows))
}

/// `files.read {worktree, relativePath}`: a text file, cut to the text cap.
pub fn read(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let relative = relative_param(params);
    let clean = confine::lexical(relative)?;
    if is_binary_path(&clean) {
        return Err(refuse("binary_file"));
    }
    let path = confine::existing(&root, &clean)?;
    let byte_length = regular_file(&path)?;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(TEXT_CAP).read_to_end(&mut bytes))
        .map_err(|_| unreadable())?;
    if bytes[..bytes.len().min(NUL_SNIFF_BYTES)].contains(&0) {
        return Err(refuse("binary_file"));
    }
    let truncated = (bytes.len() as u64) < byte_length;
    Ok(json!({
        "worktree": params["worktree"],
        "relativePath": relative,
        "content": text_of(bytes, truncated),
        "truncated": truncated,
        "byteLength": byte_length,
    }))
}

/// `files.readPreview {worktree, relativePath}`: an image as base64. An absolute `relativePath` names a staged phone
/// upload instead, served from the uploads root only (see `read_upload_preview`).
pub fn read_preview(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let relative = relative_param(params);
    if Path::new(relative).is_absolute() {
        return read_upload_preview(params, ctx);
    }
    let root = rpc::project_root(ctx, params)?;
    let clean = confine::lexical(relative)?;
    let mime = image_mime(&clean).ok_or_else(|| refuse("binary_file"))?;
    let path = confine::existing(&root, &clean)?;
    preview_reply(params, mime, &path)
}

/// An image the bridge staged from a phone upload, named by its absolute path. Only a file directly inside
/// `<bridge dir>/uploads/<conversation>/` is served: the path is canonicalised first, so a `..` or a symlink that
/// leaves the uploads root is refused, and a file nested deeper is not an upload.
fn read_upload_preview(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = std::fs::canonicalize(uploads::staging_root(&ctx.dir))
        .map_err(|_| ("not_found", "file not found".to_string()))?;
    let path = std::fs::canonicalize(relative_param(params))
        .map_err(|_| ("not_found", "file not found".to_string()))?;
    let inside = path.strip_prefix(&root).map_err(|_| confine::invalid())?;
    if inside.components().count() != 2 {
        return Err(confine::invalid());
    }
    let mime = image_mime(&path).ok_or_else(|| refuse("binary_file"))?;
    preview_reply(params, mime, &path)
}

fn preview_reply(params: &Value, mime: &'static str, path: &Path) -> Result<Value, Failure> {
    if regular_file(path)? > IMAGE_CAP {
        return Err(refuse("file_too_large"));
    }
    let bytes = std::fs::read(path).map_err(|_| unreadable())?;
    Ok(json!({
        "worktree": params["worktree"],
        "relativePath": relative_param(params),
        "content": base64::engine::general_purpose::STANDARD.encode(bytes),
        "isBinary": true,
        "isImage": true,
        "mimeType": mime,
    }))
}

/// `files.resolveTerminalPath {worktree, pathText, cwd?, crossWorkspace?, …}`: where a path in a pane points.
pub fn resolve_terminal_path(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let text = params["pathText"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or(("bad_request", "missing pathText".to_string()))?;
    let base = params["cwd"]
        .as_str()
        .filter(|c| !c.is_empty() && Path::new(c).is_absolute())
        .map_or_else(|| root.clone(), PathBuf::from);
    // A `file.rs:12:4` link names a file that exists only once its position is cut off. The exact
    // text is tried first, so a file whose real name ends in digits still resolves.
    let canonical = canonical_under(&base, text)
        .or_else(|| strip_position(text).and_then(|bare| canonical_under(&base, bare)));
    let inside = canonical.and_then(|path| confine::relative_inside(&root, &path).map(|rel| (path, rel)));
    let worktree = params["worktree"].clone();
    let Some((path, relative)) = inside.filter(|(p, _)| p.is_file() || p.is_dir()) else {
        return Ok(json!({
            "worktree": worktree,
            "relativePath": null,
            "absolutePath": null,
            "exists": false,
            "isDirectory": false,
        }));
    };
    let relative = relative.to_string_lossy().into_owned();
    let absolute = path.to_string_lossy().into_owned();
    let is_directory = path.is_dir();
    let mut result = json!({
        "worktree": worktree,
        "relativePath": relative,
        "absolutePath": absolute,
        "exists": true,
        "isDirectory": is_directory,
    });
    if !is_directory {
        result["openTarget"] = json!({"kind": "worktree-file", "relativePath": relative, "absolutePath": absolute});
    }
    Ok(result)
}

/// `files.searchPaths {worktree, query, limit?, excludePaths?, …}`: project files whose path matches the query.
/// The `mode`, `includeIgnored` and `followSymlinks` options are not read: the walk always honours
/// the project's ignore files and never follows a symlink.
pub fn search_paths(params: &Value, ctx: &Ctx) -> Result<Value, Failure> {
    let root = rpc::project_root(ctx, params)?;
    let query = params["query"].as_str().unwrap_or("").to_lowercase();
    let limit = params["limit"]
        .as_i64()
        .map_or(SEARCH_LIMIT_DEFAULT, |n| n.clamp(1, SEARCH_LIMIT_MAX)) as usize;
    let excluded: HashSet<&str> = params["excludePaths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let walk = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != OsStr::new(confine::GIT_DIR))
        .build();
    let mut hits = Vec::new();
    let mut walked = 0usize;
    let mut budget_hit = false;
    for item in walk {
        let Ok(entry) = item else { continue };
        if entry.depth() == 0 {
            continue;
        }
        if walked == WALK_BUDGET {
            budget_hit = true;
            break;
        }
        walked += 1;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else { continue };
        let relative = relative.to_string_lossy().into_owned();
        if excluded.contains(relative.as_str()) || !relative.to_lowercase().contains(&query) {
            continue;
        }
        let basename = entry.file_name().to_string_lossy().into_owned();
        let lower_basename = basename.to_lowercase();
        let rank = if lower_basename.starts_with(&query) {
            0
        } else if lower_basename.contains(&query) {
            1
        } else {
            2
        };
        let kind = if is_binary_path(entry.path()) { "binary" } else { "text" };
        hits.push(Hit { rank, relative, basename, kind });
    }
    hits.sort_by(|a, b| (a.rank, a.relative.len(), &a.relative).cmp(&(b.rank, b.relative.len(), &b.relative)));

    let total = hits.len();
    let files: Vec<Value> = hits
        .iter()
        .take(limit)
        .map(|h| json!({"relativePath": h.relative, "basename": h.basename, "kind": h.kind}))
        .collect();
    Ok(json!({
        "worktree": params["worktree"],
        "files": files,
        "totalCount": total,
        "truncated": total > limit || budget_hit,
    }))
}

/// One directory entry as `files.readDir` reports it.
struct Listed {
    name: String,
    is_directory: bool,
    is_symlink: bool,
}

/// One file the search matched, with its rank (0 basename starts with the query, 1 basename contains
/// it, 2 only the path does).
struct Hit {
    rank: u8,
    relative: String,
    basename: String,
    kind: &'static str,
}

fn relative_param(params: &Value) -> &str {
    params["relativePath"].as_str().unwrap_or("")
}

/// A refusal whose code and message are the same desktop token.
fn refuse(token: &'static str) -> Failure {
    (token, token.to_string())
}

fn unreadable() -> Failure {
    ("unavailable", "the file cannot be read".to_string())
}

/// The byte length of `path` when it is a regular file.
fn regular_file(path: &Path) -> Result<u64, Failure> {
    let meta = std::fs::metadata(path).map_err(|_| unreadable())?;
    if !meta.is_file() {
        return Err(refuse("not_a_file"));
    }
    Ok(meta.len())
}

/// Whether the file name's extension is one the phone never reads as text.
fn is_binary_path(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The MIME type of an image the preview serves, from its extension.
fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The text of a file's bytes. A file cut at the cap may end inside a character; that partial
/// character is dropped so the content is whole UTF-8.
fn text_of(bytes: Vec<u8>, truncated: bool) -> String {
    let error = match String::from_utf8(bytes) {
        Ok(text) => return text,
        Err(error) => error,
    };
    let cut_mid_character = truncated && error.utf8_error().error_len().is_none();
    let keep = if cut_mid_character {
        error.utf8_error().valid_up_to()
    } else {
        error.as_bytes().len()
    };
    String::from_utf8_lossy(&error.as_bytes()[..keep]).into_owned()
}

/// The canonical path of `text` read from `base` (an absolute `text` replaces `base`), when it exists.
fn canonical_under(base: &Path, text: &str) -> Option<PathBuf> {
    std::fs::canonicalize(base.join(text)).ok()
}

/// `text` without a trailing `:line` or `:line:column`, as compilers and grep print them.
fn strip_position(text: &str) -> Option<&str> {
    let is_number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (head, last) = text.rsplit_once(':')?;
    if !is_number(last) {
        return None;
    }
    let bare = match head.rsplit_once(':') {
        Some((inner, middle)) if is_number(middle) => inner,
        _ => head,
    };
    (!bare.is_empty()).then_some(bare)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::testkit::{conversation, ctx_with, sandbox};
    use base64::Engine;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// A context whose only conversation is a fresh canonical project directory, and its selector.
    fn project(name: &str) -> (Ctx, PathBuf, String) {
        let root = fs::canonicalize(sandbox(name)).unwrap();
        let conv = conversation(name, root.to_str().unwrap());
        let worktree = conv.worktree_id();
        let ctx = ctx_with(vec![conv], &sandbox(&format!("{name}-state")));
        (ctx, root, worktree)
    }

    fn put(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn at(worktree: &str, relative: &str) -> Value {
        json!({"worktree": worktree, "relativePath": relative})
    }

    fn code(result: Result<Value, Failure>) -> &'static str {
        match result {
            Err((code, _)) => code,
            Ok(value) => panic!("expected a refusal, got {value}"),
        }
    }

    fn names(listing: &Value) -> Vec<String> {
        listing
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn search(ctx: &Ctx, worktree: &str, extra: Value) -> Value {
        let mut params = extra;
        params["worktree"] = json!(worktree);
        search_paths(&params, ctx).unwrap()
    }

    fn paths(result: &Value) -> Vec<String> {
        result["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["relativePath"].as_str().unwrap().to_string())
            .collect()
    }

    fn resolve_at(ctx: &Ctx, worktree: &str, path_text: &str, cwd: Option<&Path>) -> Value {
        let mut params = json!({"worktree": worktree, "pathText": path_text});
        if let Some(cwd) = cwd {
            params["cwd"] = json!(cwd.to_str().unwrap());
        }
        resolve_terminal_path(&params, ctx).unwrap()
    }

    #[test]
    fn read_dir_lists_directories_first_then_names() {
        let (ctx, root, wt) = project("rd-order");
        put(&root, "b.txt", b"");
        put(&root, "A.txt", b"");
        put(&root, "zeta/x", b"");
        put(&root, "alpha/x", b"");
        let listing = read_dir(&at(&wt, ""), &ctx).unwrap();
        assert_eq!(names(&listing), vec!["alpha", "zeta", "A.txt", "b.txt"]);
        assert_eq!(listing[0]["isDirectory"], true);
        assert_eq!(listing[2]["isDirectory"], false);
    }

    #[test]
    fn read_dir_hides_git() {
        let (ctx, root, wt) = project("rd-git");
        put(&root, ".git/config", b"[core]");
        put(&root, "a.txt", b"");
        assert_eq!(names(&read_dir(&at(&wt, ""), &ctx).unwrap()), vec!["a.txt"]);
    }

    #[test]
    fn read_dir_omits_a_symlink_to_outside_and_shows_one_inside() {
        let (ctx, root, wt) = project("rd-links");
        let outside = sandbox("rd-links-outside");
        put(&outside, "secret.txt", b"x");
        put(&root, "real/f.txt", b"");
        put(&root, "a.txt", b"");
        symlink(&outside, root.join("out")).unwrap();
        symlink(root.join("a.txt"), root.join("in.txt")).unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();

        let listing = read_dir(&at(&wt, ""), &ctx).unwrap();
        assert_eq!(names(&listing), vec!["alias", "real", "a.txt", "in.txt"]);
        let alias = &listing[0];
        assert_eq!(alias["isSymlink"], true);
        assert_eq!(alias["isDirectory"], true);
        let inside = &listing[3];
        assert_eq!(inside["isSymlink"], true);
        assert_eq!(inside["isDirectory"], false);
    }

    #[test]
    fn read_dir_refuses_escapes_and_bad_targets() {
        let (ctx, root, wt) = project("rd-refuse");
        put(&root, "f.txt", b"");
        assert_eq!(code(read_dir(&at(&wt, "../x"), &ctx)), "invalid_relative_path");
        assert_eq!(code(read_dir(&at(&wt, "/etc"), &ctx)), "invalid_relative_path");
        assert_eq!(code(read_dir(&at(&wt, "nope"), &ctx)), "not_found");
        assert_eq!(code(read_dir(&at(&wt, "f.txt"), &ctx)), "not_a_directory");
    }

    #[test]
    fn read_dir_refuses_more_than_5000_entries() {
        let (ctx, root, wt) = project("rd-large");
        for i in 0..=MAX_DIR_ENTRIES {
            put(&root, &format!("f{i:05}"), b"");
        }
        assert_eq!(code(read_dir(&at(&wt, ""), &ctx)), "directory_too_large");
    }

    #[test]
    fn read_round_trips_text() {
        let (ctx, root, wt) = project("read-text");
        put(&root, "a.rs", b"fn main() {}\n");
        let result = read(&at(&wt, "a.rs"), &ctx).unwrap();
        assert_eq!(result["content"], "fn main() {}\n");
        assert_eq!(result["truncated"], false);
        assert_eq!(result["byteLength"], 13);
        assert_eq!(result["relativePath"], "a.rs");
    }

    #[test]
    fn read_cuts_at_the_text_cap_and_reports_the_full_size() {
        let (ctx, root, wt) = project("read-cap");
        let size = TEXT_CAP as usize + 10;
        put(&root, "big.txt", &vec![b'a'; size]);
        let result = read(&at(&wt, "big.txt"), &ctx).unwrap();
        assert_eq!(result["truncated"], true);
        assert_eq!(result["byteLength"], size as u64);
        assert_eq!(result["content"].as_str().unwrap().len(), TEXT_CAP as usize);
    }

    #[test]
    fn read_cut_inside_a_multibyte_character_stays_valid_utf8() {
        let (ctx, root, wt) = project("read-utf8");
        // One ASCII byte, then two-byte characters: the cap lands inside one of them.
        let mut bytes = b"a".to_vec();
        while bytes.len() < TEXT_CAP as usize + 10 {
            bytes.extend_from_slice("é".as_bytes());
        }
        put(&root, "wide.txt", &bytes);
        let result = read(&at(&wt, "wide.txt"), &ctx).unwrap();
        assert_eq!(result["truncated"], true);
        assert_eq!(result["content"].as_str().unwrap().len(), TEXT_CAP as usize - 1);
    }

    #[test]
    fn read_refuses_binary_by_extension_and_by_nul() {
        let (ctx, root, wt) = project("read-binary");
        put(&root, "photo.PNG", b"not really a png");
        put(&root, "blob.dat", b"text\0more");
        assert_eq!(code(read(&at(&wt, "photo.PNG"), &ctx)), "binary_file");
        assert_eq!(code(read(&at(&wt, "blob.dat"), &ctx)), "binary_file");
    }

    #[test]
    fn read_refuses_paths_outside_the_project_and_git() {
        let (ctx, root, wt) = project("read-confine");
        let outside = sandbox("read-confine-outside");
        put(&outside, "s.txt", b"x");
        put(&root, ".git/config", b"[remote]");
        symlink(outside.join("s.txt"), root.join("esc.txt")).unwrap();
        assert_eq!(code(read(&at(&wt, "../x"), &ctx)), "invalid_relative_path");
        assert_eq!(code(read(&at(&wt, ".git/config"), &ctx)), "invalid_relative_path");
        assert_eq!(code(read(&at(&wt, "esc.txt"), &ctx)), "invalid_relative_path");
    }

    #[test]
    fn read_refuses_a_directory() {
        let (ctx, root, wt) = project("read-dir");
        put(&root, "sub/x", b"");
        assert_eq!(code(read(&at(&wt, "sub"), &ctx)), "not_a_file");
    }

    #[test]
    fn read_preview_round_trips_a_png() {
        let (ctx, root, wt) = project("preview-png");
        let png = b"\x89PNG\r\n\x1a\n-body-";
        put(&root, "shot.png", png);
        let result = read_preview(&at(&wt, "shot.png"), &ctx).unwrap();
        assert_eq!(result["mimeType"], "image/png");
        assert_eq!(result["isBinary"], true);
        assert_eq!(result["isImage"], true);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn read_preview_refuses_svg_and_pdf() {
        let (ctx, root, wt) = project("preview-types");
        put(&root, "logo.svg", b"<svg/>");
        put(&root, "doc.pdf", b"%PDF");
        assert_eq!(code(read_preview(&at(&wt, "logo.svg"), &ctx)), "binary_file");
        assert_eq!(code(read_preview(&at(&wt, "doc.pdf"), &ctx)), "binary_file");
    }

    #[test]
    fn read_preview_refuses_an_image_over_the_cap() {
        let (ctx, root, wt) = project("preview-big");
        put(&root, "huge.png", &vec![0u8; IMAGE_CAP as usize + 1]);
        assert_eq!(code(read_preview(&at(&wt, "huge.png"), &ctx)), "file_too_large");
    }

    /// A context with no conversations and a state directory: staged uploads live under `<state>/uploads/`.
    fn staged(name: &str) -> (Ctx, PathBuf) {
        let state = fs::canonicalize(sandbox(name)).unwrap();
        (ctx_with(vec![], &state), state)
    }

    fn upload_at(path: &Path) -> Value {
        json!({"worktree": "id:none", "relativePath": path.to_str().unwrap()})
    }

    #[test]
    fn read_preview_serves_a_staged_upload_by_its_absolute_path() {
        let (ctx, state) = staged("preview-upload");
        let png = b"\x89PNG\r\n\x1a\n-body-";
        put(&state, "uploads/_unscoped/abc.png", png);
        let result = read_preview(&upload_at(&state.join("uploads/_unscoped/abc.png")), &ctx).unwrap();
        assert_eq!(result["mimeType"], "image/png");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn read_preview_refuses_an_absolute_path_that_leaves_the_uploads_root() {
        let (ctx, state) = staged("preview-upload-escape");
        put(&state, "secret.png", b"\x89PNG\r\n\x1a\n");
        put(&state, "uploads/_unscoped/ok.png", b"\x89PNG\r\n\x1a\n");
        for bad in [
            state.join("secret.png"),
            state.join("uploads/../secret.png"),
            state.join("uploads/_unscoped/../../secret.png"),
        ] {
            assert_eq!(code(read_preview(&upload_at(&bad), &ctx)), "invalid_relative_path", "{bad:?}");
        }
    }

    #[test]
    fn read_preview_refuses_an_upload_symlink_out_of_the_root_and_a_nested_file() {
        let (ctx, state) = staged("preview-upload-links");
        put(&state, "secret.png", b"\x89PNG\r\n\x1a\n");
        put(&state, "uploads/_unscoped/deep/nested.png", b"\x89PNG\r\n\x1a\n");
        symlink(state.join("secret.png"), state.join("uploads/_unscoped/link.png")).unwrap();
        let link = state.join("uploads/_unscoped/link.png");
        let nested = state.join("uploads/_unscoped/deep/nested.png");
        assert_eq!(code(read_preview(&upload_at(&link), &ctx)), "invalid_relative_path");
        assert_eq!(code(read_preview(&upload_at(&nested), &ctx)), "invalid_relative_path");
    }

    #[test]
    fn read_preview_refuses_a_partial_or_non_image_upload_and_a_missing_one() {
        let (ctx, state) = staged("preview-upload-kinds");
        put(&state, "uploads/_unscoped/abc.part", b"partial");
        put(&state, "uploads/_unscoped/logo.svg", b"<svg/>");
        let part = state.join("uploads/_unscoped/abc.part");
        let svg = state.join("uploads/_unscoped/logo.svg");
        assert_eq!(code(read_preview(&upload_at(&part), &ctx)), "binary_file");
        assert_eq!(code(read_preview(&upload_at(&svg), &ctx)), "binary_file");
        let missing = state.join("uploads/_unscoped/missing.png");
        assert_eq!(code(read_preview(&upload_at(&missing), &ctx)), "not_found");
    }

    #[test]
    fn read_preview_refuses_an_upload_over_the_cap() {
        let (ctx, state) = staged("preview-upload-big");
        put(&state, "uploads/_unscoped/huge.png", &vec![0u8; IMAGE_CAP as usize + 1]);
        let huge = state.join("uploads/_unscoped/huge.png");
        assert_eq!(code(read_preview(&upload_at(&huge), &ctx)), "file_too_large");
    }

    #[test]
    fn resolve_inside_relative_path_is_an_open_target() {
        let (ctx, root, wt) = project("resolve-rel");
        put(&root, "src/lib.rs", b"");
        let result = resolve_at(&ctx, &wt, "src/lib.rs", None);
        assert_eq!(result["exists"], true);
        assert_eq!(result["isDirectory"], false);
        assert_eq!(result["relativePath"], "src/lib.rs");
        assert_eq!(result["openTarget"]["kind"], "worktree-file");
        assert_eq!(result["openTarget"]["relativePath"], "src/lib.rs");
    }

    #[test]
    fn resolve_absolute_path_inside_the_project() {
        let (ctx, root, wt) = project("resolve-abs");
        put(&root, "src/lib.rs", b"");
        let text = root.join("src/lib.rs").to_str().unwrap().to_string();
        let result = resolve_at(&ctx, &wt, &text, None);
        assert_eq!(result["relativePath"], "src/lib.rs");
        assert_eq!(result["openTarget"]["relativePath"], "src/lib.rs");
    }

    #[test]
    fn resolve_absolute_path_outside_the_project_does_not_exist() {
        let (ctx, _root, wt) = project("resolve-out");
        let outside = sandbox("resolve-out-target");
        put(&outside, "o.rs", b"");
        let text = outside.join("o.rs").to_str().unwrap().to_string();
        let result = resolve_at(&ctx, &wt, &text, None);
        assert_eq!(result["exists"], false);
        assert_eq!(result["isDirectory"], false);
        assert!(result["relativePath"].is_null());
        assert!(result["absolutePath"].is_null());
        assert!(result.get("openTarget").is_none());
    }

    #[test]
    fn resolve_parent_escape_does_not_exist() {
        let (ctx, _root, wt) = project("resolve-up");
        let outside = sandbox("resolve-up-outside");
        put(&outside, "x.txt", b"");
        let text = format!("../{}/x.txt", outside.file_name().unwrap().to_str().unwrap());
        let result = resolve_at(&ctx, &wt, &text, None);
        assert_eq!(result["exists"], false);
        assert!(result["relativePath"].is_null());
    }

    #[test]
    fn resolve_strips_a_line_and_column_suffix() {
        let (ctx, root, wt) = project("resolve-pos");
        put(&root, "file.rs", b"");
        let with_column = resolve_at(&ctx, &wt, "file.rs:12:4", None);
        assert_eq!(with_column["exists"], true);
        assert_eq!(with_column["relativePath"], "file.rs");
        let with_line = resolve_at(&ctx, &wt, "file.rs:12", None);
        assert_eq!(with_line["relativePath"], "file.rs");
    }

    #[test]
    fn resolve_directory_has_no_open_target() {
        let (ctx, root, wt) = project("resolve-dir");
        put(&root, "sub/x", b"");
        let result = resolve_at(&ctx, &wt, "sub", None);
        assert_eq!(result["exists"], true);
        assert_eq!(result["isDirectory"], true);
        assert!(result.get("openTarget").is_none());
    }

    #[test]
    fn resolve_leaves_a_tilde_as_a_literal_name() {
        let (ctx, root, wt) = project("resolve-tilde");
        put(&root, "~/x", b"");
        let result = resolve_at(&ctx, &wt, "~/x", None);
        assert_eq!(result["exists"], true);
        assert_eq!(result["relativePath"], "~/x");
    }

    #[test]
    fn resolve_joins_a_relative_path_onto_the_cwd() {
        let (ctx, root, wt) = project("resolve-cwd");
        put(&root, "sub/f.txt", b"");
        let sub = root.join("sub");
        let result = resolve_at(&ctx, &wt, "f.txt", Some(sub.as_path()));
        assert_eq!(result["exists"], true);
        assert_eq!(result["relativePath"], "sub/f.txt");
    }

    #[test]
    fn search_omits_gitignored_files() {
        let (ctx, root, wt) = project("search-ignore");
        put(&root, ".gitignore", b"secret.txt\n");
        put(&root, "secret.txt", b"");
        put(&root, "public.txt", b"");
        let found = paths(&search(&ctx, &wt, json!({"query": ""})));
        assert!(!found.iter().any(|p| p == "secret.txt"));
        assert!(found.iter().any(|p| p == "public.txt"));
    }

    #[test]
    fn search_honours_a_nested_gitignore_only_below_it() {
        let (ctx, root, wt) = project("search-nested");
        put(&root, "sub/.gitignore", b"*.log\n");
        put(&root, "sub/a.log", b"");
        put(&root, "sub/b.txt", b"");
        put(&root, "c.log", b"");
        let found = paths(&search(&ctx, &wt, json!({"query": ""})));
        assert!(!found.iter().any(|p| p == "sub/a.log"));
        assert!(found.iter().any(|p| p == "sub/b.txt"));
        assert!(found.iter().any(|p| p == "c.log"));
    }

    #[test]
    fn search_never_lists_git_contents() {
        let (ctx, root, wt) = project("search-git");
        put(&root, ".git/config", b"[remote]");
        put(&root, ".git/HEAD", b"ref");
        put(&root, "readme.md", b"");
        let found = paths(&search(&ctx, &wt, json!({"query": ""})));
        assert_eq!(found, vec!["readme.md"]);
    }

    #[test]
    fn search_does_not_follow_symlinks_out_of_the_project() {
        let (ctx, root, wt) = project("search-symlink");
        let outside = sandbox("search-symlink-outside");
        put(&outside, "leak.txt", b"");
        symlink(&outside, root.join("link")).unwrap();
        symlink(outside.join("leak.txt"), root.join("alias.txt")).unwrap();
        assert_eq!(search(&ctx, &wt, json!({"query": "leak"}))["totalCount"], 0);
        assert_eq!(search(&ctx, &wt, json!({"query": "alias"}))["totalCount"], 0);
    }

    #[test]
    fn search_ranks_basename_prefix_first() {
        let (ctx, root, wt) = project("search-rank");
        put(&root, "libs/readme.md", b"");
        put(&root, "my_lib.rs", b"");
        put(&root, "lib.rs", b"");
        let found = paths(&search(&ctx, &wt, json!({"query": "lib"})));
        assert_eq!(found, vec!["lib.rs", "my_lib.rs", "libs/readme.md"]);
    }

    #[test]
    fn search_limit_caps_rows_and_sets_truncated() {
        let (ctx, root, wt) = project("search-limit");
        for i in 1..=5 {
            put(&root, &format!("f{i}.txt"), b"");
        }
        let result = search(&ctx, &wt, json!({"query": "f", "limit": 2}));
        assert_eq!(paths(&result).len(), 2);
        assert_eq!(result["totalCount"], 5);
        assert_eq!(result["truncated"], true);
        let all = search(&ctx, &wt, json!({"query": "f", "limit": 500}));
        assert_eq!(paths(&all).len(), 5);
        assert_eq!(all["truncated"], false);
    }

    #[test]
    fn search_exclude_paths_drops_an_exact_match() {
        let (ctx, root, wt) = project("search-exclude");
        put(&root, "a.txt", b"");
        put(&root, "b.txt", b"");
        let result = search(&ctx, &wt, json!({"query": "", "excludePaths": ["a.txt"]}));
        assert_eq!(paths(&result), vec!["b.txt"]);
        assert_eq!(result["totalCount"], 1);
        assert_eq!(result["truncated"], false);
    }

    #[test]
    fn search_lists_files_only_and_marks_binary_kind() {
        let (ctx, root, wt) = project("search-kind");
        fs::create_dir_all(root.join("emptydir")).unwrap();
        put(&root, "pic.png", b"");
        let dirs = search(&ctx, &wt, json!({"query": "emptydir"}));
        assert_eq!(dirs["totalCount"], 0);
        let result = search(&ctx, &wt, json!({"query": "pic"}));
        assert_eq!(result["files"][0]["kind"], "binary");
        assert_eq!(result["files"][0]["basename"], "pic.png");
    }

    #[test]
    fn live_explorer_over_a_git_repository() {
        let (ctx, root, wt) = project("live");
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        put(&root, ".gitignore", b"target/\n*.tmp\n");
        put(&root, ".git/info/exclude", b"private.txt\n");
        put(&root, "src/main.rs", b"fn main() {}\n");
        put(&root, "src/gen/.gitignore", b"out.rs\n");
        put(&root, "src/gen/out.rs", b"");
        put(&root, "src/gen/keep.rs", b"");
        put(&root, "target/debug/app", b"");
        put(&root, "scratch.tmp", b"");
        put(&root, "private.txt", b"");
        put(&root, "notes/todo.md", b"");
        let outside = sandbox("live-outside");
        put(&outside, "leak.md", b"x");
        symlink(&outside, root.join("external")).unwrap();

        let top = read_dir(&at(&wt, ""), &ctx).unwrap();
        assert_eq!(
            names(&top),
            vec!["notes", "src", "target", ".gitignore", "private.txt", "scratch.tmp"]
        );
        assert_eq!(names(&read_dir(&at(&wt, "src"), &ctx).unwrap()), vec!["gen", "main.rs"]);

        let found = paths(&search(&ctx, &wt, json!({"query": ""})));
        for expected in ["src/main.rs", "src/gen/keep.rs", "notes/todo.md", ".gitignore"] {
            assert!(found.iter().any(|p| p == expected), "missing {expected}");
        }
        for hidden in ["target/debug/app", "scratch.tmp", "private.txt", "src/gen/out.rs", "external/leak.md"] {
            assert!(!found.iter().any(|p| p == hidden), "listed {hidden}");
        }
    }
}
