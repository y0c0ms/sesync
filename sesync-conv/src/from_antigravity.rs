//! Antigravity conversation DB (`<store>/conversations/<id>.db`, CLI or App) -> omp journal.
//!
//! Reads a consistent in-memory snapshot (WAL included), checks the structure
//! (required tables and columns, a trajectory with source 17 = CLI or 1 = App),
//! then maps the steps in `idx` order:
//!   header          id = cascade_id (else file stem), cwd = TrajectoryMetadata 1.1 or 7,
//!                   timestamp = TM 2, title = annotations/<id>.pbtxt, else the
//!                   conversation_summaries row
//!   14 user input   -> user: text 19.2 (else 19.1, else the 19.3 items); images 19.9
//!                      (inline bytes, else the field-5 file, else the JPEG thumbnail)
//!   15 planner      -> assistant: thinking 20.3, text 20.1 (else 20.8), toolCall per 20.7
//!   132 generic     -> toolResult: call from StepMetadata 4 (payload 5.4, else the metadata
//!                      column), else the next pending call; text 140.2.1; isError on
//!                      status 4/7 or an error field 31 (whose text is used when 140.2 is empty)
//!   21 run_command, 8 view_file, 38 mcp, 33 search_web (older builds, the App) -> toolResult
//!      from their known output field; 5, 7, 9, 31 and other tool steps -> a toolResult note
//!      saying the output was not converted (or the error text), counted as opaque
//!   98, 101, 23, 17 -> skipped and counted (runtime and system steps)
//! Every message keeps its step's timestamp (StepMetadata 1).

use crate::antigravity::{self as ag, file_uri_to_path};
use crate::journal::iso_from_ms;
use crate::protobuf::{timestamp_ms, Msg};
use base64::Engine;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub thinking_blocks: usize,
    pub images: usize,
    pub missing_images: usize,
    pub unlinked_tool_results: usize,
    pub opaque_steps: usize,
    pub skipped_system_steps: usize,
    pub skipped_empty_steps: usize,
    pub bad_payloads: usize,
}

struct Row {
    idx: i64,
    step_type: i64,
    status: i64,
    metadata: Option<Vec<u8>>,
    payload: Option<Vec<u8>>,
}

/// Header facts read from the DB and the store around it.
#[derive(Debug, Default)]
pub struct Header {
    pub id: String,
    pub cwd: String,
    pub start_ms: Option<i64>,
    pub title: Option<String>,
}

fn read_title(db: &Path, id: &str) -> Option<String> {
    let store = ag::store_of(db)?;
    if let Some(t) = fs::read_to_string(ag::annotation_path(&store, id)).ok().and_then(|s| ag::parse_annotation_title(&s)) {
        return Some(t);
    }
    let sum = store.join("conversation_summaries.db");
    if !sum.is_file() {
        return None;
    }
    let c = Connection::open_with_flags(&sum, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = c.busy_timeout(std::time::Duration::from_secs(2));
    c.query_row("SELECT title FROM conversation_summaries WHERE conversation_id = ?1", [id], |r| r.get::<_, String>(0))
        .ok()
        .filter(|t| !t.trim().is_empty())
}

fn read_header(c: &Connection, db: &Path) -> Result<Header, String> {
    let rows: Vec<(Option<String>, Option<i64>, Option<i64>)> = {
        let mut st = c
            .prepare("SELECT cascade_id, trajectory_type, source FROM trajectory_meta")
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?;
        it.filter_map(Result::ok).collect()
    };
    let Some((cascade, _, _)) = rows
        .iter()
        .find(|(_, t, s)| *t == Some(ag::TRAJECTORY_TYPE) && matches!(*s, Some(ag::CLI_SOURCE) | Some(ag::APP_SOURCE)))
    else {
        let seen: Vec<String> = rows.iter().map(|(_, t, s)| format!("type {t:?} source {s:?}")).collect();
        return Err(format!(
            "not an Antigravity CLI or App conversation: no trajectory with type 4 and source 17 or 1 (found: {})",
            if seen.is_empty() { "none".into() } else { seen.join(", ") }
        ));
    };
    let stem = db.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let id = cascade.clone().filter(|s| !s.is_empty()).unwrap_or(stem);
    let tm: Option<Vec<u8>> = c
        .query_row("SELECT data FROM trajectory_metadata_blob WHERE id = 'main'", [], |r| r.get(0))
        .ok()
        .flatten();
    let mut h = Header { id, ..Default::default() };
    if let Some(tm) = tm.as_deref() {
        let m = Msg::lenient(tm);
        let uri = m.message(1).and_then(|w| w.text(1)).or_else(|| m.text(7));
        if let Some(u) = uri {
            h.cwd = file_uri_to_path(&u);
        }
        h.start_ms = m.message(2).as_ref().and_then(timestamp_ms);
    }
    h.title = read_title(db, &h.id);
    Ok(h)
}

fn read_rows(c: &Connection) -> Result<Vec<Row>, String> {
    let mut st = c
        .prepare("SELECT idx, step_type, status, metadata, step_payload FROM steps ORDER BY idx")
        .map_err(|e| e.to_string())?;
    let it = st
        .query_map([], |r| {
            Ok(Row { idx: r.get(0)?, step_type: r.get(1)?, status: r.get(2)?, metadata: r.get(3)?, payload: r.get(4)? })
        })
        .map_err(|e| e.to_string())?;
    it.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

struct Out {
    records: Vec<Map<String, Value>>,
    last: Option<String>,
}

impl Out {
    fn push(&mut self, id: String, ts: &str, message: Value, st: &mut Stats) {
        let mut m = Map::new();
        m.insert("type".into(), "message".into());
        m.insert("id".into(), id.clone().into());
        m.insert("parentId".into(), self.last.clone().map(Value::from).unwrap_or(Value::Null));
        m.insert("timestamp".into(), ts.into());
        m.insert("message".into(), message);
        self.records.push(m);
        self.last = Some(id);
        st.messages += 1;
    }
}

fn tool_result(id: &str, name: &str, text: &str, is_error: bool) -> Value {
    json!({"role": "toolResult", "toolCallId": id, "toolName": name, "content": [{"type": "text", "text": text}], "isError": is_error})
}

/// ToolCall {1 id, 2 name, 3 args JSON} -> (id, name, arguments object).
fn decode_call(m: &Msg) -> (String, String, Value) {
    let id = m.string(1).unwrap_or_default();
    let name = m.text(2).unwrap_or_else(|| "unknown_tool".into());
    let raw = m.string(3).unwrap_or_default();
    let args = match serde_json::from_str::<Value>(&raw) {
        Ok(v @ Value::Object(_)) => v,
        Ok(Value::Null) | Err(_) if raw.trim().is_empty() => json!({}),
        Ok(v) => json!({"input": v}),
        Err(_) => json!({"input": raw}),
    };
    (id, name, args)
}

/// Tool step types of older builds and the App (each has its own payload field).
const DEDICATED_TOOL_STEPS: &[i64] = &[5, 7, 8, 9, 21, 31, 33, 38];

/// The output text of an older dedicated tool step, from the field the
/// session-migrate decoders and the App data use. None if it has none.
fn dedicated_output(step_type: i64, p: &Msg) -> Option<String> {
    let first = |sub: Option<Msg>, fields: &[u32]| sub.and_then(|s| fields.iter().find_map(|f| s.text(*f)));
    match step_type {
        21 => {
            let s = p.message(28)?;
            s.message(21).and_then(|o| o.text(1)).or_else(|| s.message(26).and_then(|o| o.text(1))).or_else(|| first(Some(s), &[4, 5]))
        }
        8 => first(p.message(14), &[4, 9]),
        38 => first(p.message(47), &[3, 8]),
        33 => first(p.message(42), &[5]),
        _ => None,
    }
}

/// Convert one conversation DB. Returns the omp journal lines, header first.
pub fn to_journal_lines(db: &Path) -> Result<(Vec<String>, Stats), String> {
    let c = ag::snapshot(db)?;
    ag::validate_schema(&c).map_err(|e| format!("{}: {e}", db.display()))?;
    let h = read_header(&c, db).map_err(|e| format!("{}: {e}", db.display()))?;
    let rows = read_rows(&c)?;
    drop(c);

    let mut st = Stats::default();
    let mut out = Out { records: Vec::new(), last: None };
    let mut pending: VecDeque<(String, String)> = VecDeque::new();
    let b64 = base64::engine::general_purpose::STANDARD;
    // Never the epoch: without any recorded time, the file's own mtime.
    let file_ms = fs::metadata(db)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let mut last_ms = h.start_ms.or(file_ms);

    for r in &rows {
        if matches!(r.step_type, ag::STEP_CONVERSATION_HISTORY | ag::STEP_SYSTEM_MESSAGE | ag::STEP_CHECKPOINT | ag::STEP_ERROR_MESSAGE) {
            st.skipped_system_steps += 1;
            continue;
        }
        let Some(p) = r.payload.as_deref().and_then(|b| Msg::decode(b).ok()) else {
            st.bad_payloads += 1;
            continue;
        };
        let md_col = r.metadata.as_deref().map(Msg::lenient);
        let md_payload = p.message(5);
        let md = |f: u32| md_payload.as_ref().and_then(|m| m.message(f)).or_else(|| md_col.as_ref().and_then(|m| m.message(f)));
        let ms = md(1).as_ref().and_then(timestamp_ms).or(last_ms);
        last_ms = ms.or(last_ms);
        let ts = ms.map(iso_from_ms).unwrap_or_default();
        let id = format!("agy-{:06}", r.idx);

        match r.step_type {
            ag::STEP_USER_INPUT => {
                let Some(u) = p.message(19) else {
                    st.skipped_empty_steps += 1;
                    continue;
                };
                let text = u.text(2).or_else(|| u.text(1)).or_else(|| {
                    let items: Vec<String> = u.all_bytes(3).into_iter().filter_map(|b| Msg::lenient(b).text(1)).collect();
                    Some(items.join("\n")).filter(|s| !s.is_empty())
                });
                let mut content = Vec::new();
                if let Some(t) = text {
                    content.push(json!({"type": "text", "text": t}));
                }
                for media in u.all_bytes(9) {
                    let m = Msg::lenient(media);
                    let mut mime = m.text(1).unwrap_or_else(|| "image/png".into());
                    let bytes = match m.bytes(2).filter(|b| !b.is_empty()) {
                        Some(b) => Some(b.to_vec()),
                        None => match m.text(5).and_then(|path| fs::read(path).ok()) {
                            Some(b) => Some(b),
                            None => m.bytes(6).filter(|b| !b.is_empty()).map(|b| {
                                mime = "image/jpeg".into();
                                b.to_vec()
                            }),
                        },
                    };
                    match bytes {
                        Some(b) => {
                            content.push(json!({"type": "image", "data": b64.encode(b), "mimeType": mime}));
                            st.images += 1;
                        }
                        None => st.missing_images += 1,
                    }
                }
                if content.is_empty() {
                    st.skipped_empty_steps += 1;
                    continue;
                }
                out.push(id, &ts, json!({"role": "user", "content": content}), &mut st);
                st.user_messages += 1;
            }
            ag::STEP_PLANNER_RESPONSE => {
                let Some(a) = p.message(20) else {
                    st.skipped_empty_steps += 1;
                    continue;
                };
                let mut content = Vec::new();
                if let Some(t) = a.text(3) {
                    content.push(json!({"type": "thinking", "thinking": t}));
                    st.thinking_blocks += 1;
                }
                if let Some(t) = a.text(1).or_else(|| a.text(8)) {
                    content.push(json!({"type": "text", "text": t}));
                }
                for (n, cb) in a.all_bytes(7).into_iter().enumerate() {
                    let (mut cid, name, args) = decode_call(&Msg::lenient(cb));
                    if cid.is_empty() {
                        cid = format!("agy-{}-{n}", r.idx);
                    }
                    pending.push_back((cid.clone(), name.clone()));
                    content.push(json!({"type": "toolCall", "id": cid, "name": name, "arguments": args}));
                    st.tool_calls += 1;
                }
                if content.is_empty() {
                    st.skipped_empty_steps += 1;
                    continue;
                }
                out.push(id, &ts, json!({"role": "assistant", "content": content}), &mut st);
                st.assistant_messages += 1;
            }
            _ => {
                // A tool step: GENERIC (1.4.4), an older dedicated type, or any step that
                // carries a ToolCall link. Anything else is runtime state we cannot map.
                let link = md(4).map(|m| decode_call(&m));
                if r.step_type != ag::STEP_GENERIC && !DEDICATED_TOOL_STEPS.contains(&r.step_type) && link.is_none() {
                    st.opaque_steps += 1;
                    continue;
                }
                let (cid, name) = match link.filter(|(id, _, _)| !id.is_empty()) {
                    Some((lid, lname, _)) => match pending.iter().position(|(p, _)| *p == lid) {
                        Some(i) => pending.remove(i).unwrap(),
                        // linked to a call this journal never showed: keep the link anyway
                        None => (lid, lname),
                    },
                    None => match pending.pop_front() {
                        Some(x) => x,
                        None => {
                            st.unlinked_tool_results += 1;
                            continue;
                        }
                    },
                };
                let err = p.message(31);
                let err_text = err.as_ref().and_then(|e| e.text(2).or_else(|| e.text(3)).or_else(|| e.text(1)));
                let is_error = matches!(r.status, ag::STATUS_INVALID | ag::STATUS_ERROR) || err.is_some();
                let text = if r.step_type == ag::STEP_GENERIC {
                    p.message(140).and_then(|g| g.message(2)).and_then(|res| res.text(1))
                } else {
                    dedicated_output(r.step_type, &p)
                };
                let text = match (text, err_text) {
                    (Some(t), _) => t,
                    (None, Some(e)) => e,
                    (None, None) if r.step_type == ag::STEP_GENERIC => String::new(),
                    (None, None) => {
                        st.opaque_steps += 1;
                        format!("[Antigravity step type {} ({name}): output not converted]", r.step_type)
                    }
                };
                out.push(id, &ts, tool_result(&cid, &name, &text, is_error), &mut st);
                st.tool_results += 1;
            }
        }
    }

    if st.user_messages + st.assistant_messages == 0 {
        return Err(format!("{}: empty conversation (no user or assistant step)", db.display()));
    }
    let header_ms = h
        .start_ms
        .or_else(|| out.records.first().and_then(|r| r.get("timestamp").and_then(Value::as_str)).and_then(crate::journal::parse_iso_ms))
        .or(file_ms);
    let mut header = Map::new();
    header.insert("type".into(), "session".into());
    header.insert("version".into(), 3.into());
    header.insert("id".into(), h.id.into());
    header.insert("timestamp".into(), header_ms.map(iso_from_ms).unwrap_or_default().into());
    header.insert("cwd".into(), h.cwd.into());
    if let Some(t) = h.title {
        header.insert("title".into(), t.into());
    }
    let mut lines = vec![serde_json::to_string(&header).map_err(|e| e.to_string())?];
    for m in &out.records {
        lines.push(serde_json::to_string(m).map_err(|e| e.to_string())?);
    }
    Ok((lines, st))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::{timestamp, Writer};
    use crate::to_antigravity::tests::scratch;
    use crate::to_antigravity::{write_db, Step};

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("agy-1.4.4-read-file.db")
    }

    fn parsed(lines: &[String]) -> Vec<Value> {
        lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// A conversation agy 1.4.4 wrote itself: prompt, planner read_file call, GENERIC result, reply.
    #[test]
    fn reads_a_native_agy_conversation_without_touching_it() {
        let db = fixture();
        let before = fs::read(&db).unwrap();
        let (lines, st) = to_journal_lines(&db).unwrap();
        let v = parsed(&lines);
        assert_eq!(v[0]["id"], "28b4f6a2-2ecf-4ad2-933f-25dfd05896bf", "header id is the cascade id");
        assert_eq!(v[0]["cwd"], r"C:\Users\manuesantos\Downloads\agy-research\sb\proj one");
        assert_eq!(v[0]["timestamp"], iso_from_ms(1_792_591_870_521));
        assert_eq!(v.len(), 5);
        assert_eq!(v[1]["message"]["content"][0]["text"], "TOOLREAD please read hello");
        let call = &v[2]["message"]["content"][0];
        assert_eq!(call["type"], "toolCall");
        assert_eq!(call["id"], "028524e9-1a76-4d75-b201-8870cd732233");
        assert_eq!(call["name"], "read_file");
        assert_eq!(call["arguments"]["Path"], r"C:\Users\manuesantos\Downloads\agy-research\sb\proj one\hello.txt");
        let res = &v[3]["message"];
        assert_eq!(res["toolCallId"], call["id"]);
        assert_eq!(res["isError"], false);
        assert!(res["content"][0]["text"].as_str().unwrap().contains("HELLO_FILE_CONTENT line one"));
        assert_eq!(v[4]["message"]["content"][0]["text"], "Tool finished. MOCK_DONE");
        assert_eq!(v[3]["timestamp"], iso_from_ms(1_792_591_870_725), "step time from StepMetadata 1");
        assert_eq!((st.tool_calls, st.tool_results, st.unlinked_tool_results, st.opaque_steps), (1, 1, 0, 0));
        assert_eq!(fs::read(&db).unwrap(), before, "the source file is not modified");
        for s in ["-wal", "-shm", "-journal"] {
            assert!(!ag::with_suffix(&db, s).exists(), "no {s} left next to the source");
        }
    }

    fn meta(ms: i64, link: Option<(&str, &str)>) -> Writer {
        let mut m = Writer::new();
        m.message(1, &timestamp(ms)).varint(3, 2);
        if let Some((id, name)) = link {
            let mut c = Writer::new();
            c.string(1, id).string(2, name).string(3, "{}");
            m.message(4, &c);
        }
        m
    }

    /// A step whose payload field 5 is `md` and whose metadata column is `column`.
    fn raw_step(ty: i64, status: i64, md: &Writer, column: Option<&Writer>, body: &[(u32, &Writer)]) -> Step {
        let mut p = Writer::new();
        p.varint(1, ty as u64).varint(4, status as u64).message(5, md);
        for (f, b) in body {
            p.message(*f, b);
        }
        Step { step_type: ty, status, metadata: column.map(Writer::finish).unwrap_or_default(), payload: p.into_bytes() }
    }

    fn planner_calls(ids: &[(&str, &str)]) -> Writer {
        let mut a = Writer::new();
        for (id, name) in ids {
            let mut c = Writer::new();
            c.string(1, id).string(2, name).string(3, r#"{"x":1}"#);
            a.message(7, &c);
        }
        a.varint(12, 2);
        a
    }

    fn user(text: &str) -> Writer {
        let mut u = Writer::new();
        u.string(2, text);
        u
    }

    fn db_with(dir: &Path, steps: &[Step]) -> std::path::PathBuf {
        let conv = dir.join("store").join("conversations");
        fs::create_dir_all(&conv).unwrap();
        let db = conv.join("c1.db");
        write_db(&db, "c1", r"C:\w s", 1_000, steps).unwrap();
        db
    }

    fn text_msg(field: u32, text: &str) -> Writer {
        let mut w = Writer::new();
        w.string(field, text);
        w
    }

    #[test]
    fn links_results_by_metadata_then_by_order_and_maps_older_step_types() {
        let dir = scratch("from-steps");
        let mut run = Writer::new();
        run.message(21, &text_msg(1, "command output"));
        let mut generic = Writer::new();
        generic.message(2, &text_msg(1, "generic out"));
        let calls = planner_calls(&[("a", "read"), ("b", "list_dir"), ("c", "run_command"), ("g", "grep_search")]);
        let steps = vec![
            raw_step(14, 3, &meta(1_000, None), None, &[(19, &user("go"))]),
            raw_step(15, 3, &meta(2_000, None), None, &[(20, &calls)]),
            // no link anywhere: the first pending call ("a")
            raw_step(132, 3, &meta(3_000, None), None, &[(140, &generic)]),
            // linked on the metadata column only; older LIST_DIRECTORY without output
            raw_step(9, 3, &meta(3_500, None), Some(&meta(3_500, Some(("b", "list_dir")))), &[]),
            // older RUN_COMMAND, output in 28.21.1
            raw_step(21, 3, &meta(4_000, Some(("c", "run_command"))), None, &[(28, &run)]),
            // older GREP_SEARCH that failed: error text in field 31.2
            raw_step(7, 7, &meta(5_000, Some(("g", "grep_search"))), None, &[(31, &text_msg(2, "grep failed"))]),
            raw_step(101, 3, &meta(6_000, None), None, &[]),
            raw_step(86, 3, &meta(6_500, None), None, &[]),
            // nothing pending, no link: cannot be placed
            raw_step(132, 3, &meta(7_000, None), None, &[(140, &generic)]),
        ];
        let db = db_with(&dir, &steps);
        let (lines, st) = to_journal_lines(&db).unwrap();
        let v = parsed(&lines);
        let results: Vec<(String, String, bool)> = v
            .iter()
            .filter(|r| r["message"]["role"] == "toolResult")
            .map(|r| {
                let m = &r["message"];
                (m["toolCallId"].as_str().unwrap().into(), m["content"][0]["text"].as_str().unwrap().into(), m["isError"].as_bool().unwrap())
            })
            .collect();
        let want: Vec<(String, String, bool)> = [
            ("a", "generic out", false),
            ("b", "[Antigravity step type 9 (list_dir): output not converted]", false),
            ("c", "command output", false),
            ("g", "grep failed", true),
        ]
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), *c))
        .collect();
        assert_eq!(results, want);
        assert_eq!(st.unlinked_tool_results, 1);
        assert_eq!(st.skipped_system_steps, 1, "the SYSTEM_MESSAGE step");
        assert_eq!(st.opaque_steps, 2, "the unconverted LIST_DIRECTORY output and the unknown step 86");
        assert_eq!(v[0]["cwd"], r"C:\w s");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn images_fall_back_from_inline_bytes_to_the_file_to_the_thumbnail() {
        let dir = scratch("from-images");
        let file = dir.join("pasted image.png");
        fs::write(&file, b"from-file").unwrap();
        let media = |inline: Option<&[u8]>, path: Option<&str>, thumb: Option<&[u8]>| {
            let mut m = Writer::new();
            m.string(1, "image/png");
            if let Some(b) = inline {
                m.bytes(2, b);
            }
            if let Some(p) = path {
                m.string(5, p);
            }
            if let Some(t) = thumb {
                m.bytes(6, t);
            }
            m
        };
        let mut u = user("see");
        u.message(9, &media(Some(b"inline"), Some("C:/nope.png"), None));
        u.message(9, &media(None, Some(&file.to_string_lossy()), Some(b"thumb")));
        u.message(9, &media(None, Some("C:/missing/x.png"), Some(b"thumb")));
        u.message(9, &media(None, None, None));
        let db = db_with(&dir, &[raw_step(14, 3, &meta(1_000, None), None, &[(19, &u)])]);
        let (lines, st) = to_journal_lines(&db).unwrap();
        let v = parsed(&lines);
        let c = &v[1]["message"]["content"];
        let b64 = base64::engine::general_purpose::STANDARD;
        let got: Vec<(Vec<u8>, &str)> = (1..4).map(|i| (b64.decode(c[i]["data"].as_str().unwrap()).unwrap(), c[i]["mimeType"].as_str().unwrap())).collect();
        assert_eq!(got, vec![(b"inline".to_vec(), "image/png"), (b"from-file".to_vec(), "image/png"), (b"thumb".to_vec(), "image/jpeg")]);
        assert_eq!((st.images, st.missing_images), (3, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_files_that_are_not_agy_conversations_but_allows_extra_columns() {
        let dir = scratch("from-reject");
        let db = db_with(&dir, &[raw_step(14, 3, &meta(1_000, None), None, &[(19, &user("hi"))])]);
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("ALTER TABLE steps ADD COLUMN future_col blob; ALTER TABLE trajectory_meta ADD COLUMN extra text;").unwrap();
        }
        assert!(to_journal_lines(&db).is_ok(), "a newer agy may add columns");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("UPDATE trajectory_meta SET source = 99").unwrap();
        }
        let e = to_journal_lines(&db).unwrap_err();
        assert!(e.contains("source 17 or 1"), "{e}");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch("UPDATE trajectory_meta SET source = 1; ALTER TABLE steps DROP COLUMN metadata;").unwrap();
        }
        let e = to_journal_lines(&db).unwrap_err();
        assert!(e.contains("steps.metadata is missing"), "{e}");
        let plain = dir.join("plain.db");
        Connection::open(&plain).unwrap().execute_batch("CREATE TABLE t (x)").unwrap();
        assert!(to_journal_lines(&plain).unwrap_err().contains("trajectory_meta is missing"));
        let empty = db_with(&scratch("from-empty"), &[raw_step(101, 3, &meta(1_000, None), None, &[])]);
        assert!(to_journal_lines(&empty).unwrap_err().contains("empty conversation"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// An App conversation (source 1) writes `file:///c%3A/...` in field 7 only and has no field 6.
    #[test]
    fn reads_app_workspace_uri_and_title_from_the_store() {
        let dir = scratch("from-app");
        let db = db_with(&dir, &[raw_step(14, 3, &meta(1_000, None), None, &[(19, &user("hi"))])]);
        {
            let c = Connection::open(&db).unwrap();
            let mut tm = Writer::new();
            tm.message(2, &timestamp(42_000)).string(7, "file:///c%3A/Users/me/mcp%20test");
            c.execute("UPDATE trajectory_metadata_blob SET data = ?1", [tm.into_bytes()]).unwrap();
            c.execute_batch("UPDATE trajectory_meta SET source = 1").unwrap();
        }
        let store = dir.join("store");
        let sum = Connection::open(store.join("conversation_summaries.db")).unwrap();
        sum.execute_batch("CREATE TABLE conversation_summaries (conversation_id text, title text); INSERT INTO conversation_summaries VALUES ('c1', 'From summary');").unwrap();
        drop(sum);
        let v = parsed(&to_journal_lines(&db).unwrap().0);
        assert_eq!(v[0]["cwd"], r"C:\Users\me\mcp test");
        assert_eq!(v[0]["timestamp"], iso_from_ms(42_000));
        assert_eq!(v[0]["title"], "From summary", "no annotations: the summary title");
        fs::create_dir_all(store.join("annotations")).unwrap();
        fs::write(store.join("annotations").join("c1.pbtxt"), "title:\"From \\\"annotations\\\"\"\nlast_user_view_time:{seconds:1 nanos:0}").unwrap();
        let v = parsed(&to_journal_lines(&db).unwrap().0);
        assert_eq!(v[0]["title"], "From \"annotations\"", "annotations win");
        let _ = fs::remove_dir_all(&dir);
    }
}
