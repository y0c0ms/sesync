//! Push notifications to an ntfy topic (`https://ntfy.sh`-style server), for when the phone is not
//! connected. The token is read once at startup from a shell command (`sh -c`), so it never sits in
//! the config file or on a command line. It is sent only as an `Authorization: Bearer` header and is
//! never part of an error message or a log line.

use base64::Engine;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Longest a publish may take before it is abandoned. Publishes run on their own thread, so this
/// bounds that thread rather than anything the phone waits for.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Longest the token command may run at startup before it is killed.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);

/// A validated ntfy target: the server's base URL, the topic, and the bearer token.
#[derive(Clone)]
pub struct Config {
    base: String,
    topic: String,
    token: String,
}

/// Validate the target and run `token_command` once with `sh -c`. Its trimmed stdout is the token.
/// The error text never contains the token, and the command's stderr is discarded so it cannot leak
/// into the log either.
pub fn load(url: &str, topic: &str, token_command: &str) -> Result<Config, String> {
    let base = url.trim().trim_end_matches('/').to_string();
    let host = base.strip_prefix("https://").or_else(|| base.strip_prefix("http://"));
    match host {
        Some(h) if !h.is_empty() && !h.contains(char::is_whitespace) => {}
        _ => return Err("the ntfy URL must be http:// or https:// followed by a host".into()),
    }
    if topic.is_empty() || topic.chars().any(|c| c == '/' || c == '?' || c == '#' || c.is_whitespace() || c.is_control()) {
        return Err("the ntfy topic must be non-empty and contain no '/', '?', '#' or whitespace".into());
    }
    if token_command.trim().is_empty() {
        return Err("the ntfy token command is empty".into());
    }
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(token_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run the ntfy token command: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("could not read the ntfy token command's output")?;
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= TOKEN_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the ntfy token command timed out".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("could not wait for the ntfy token command: {e}")),
        }
    };
    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("the ntfy token command failed (exit code {code})"),
            None => "the ntfy token command was killed by a signal".into(),
        });
    }
    let output = reader.join().unwrap_or_default();
    let token = String::from_utf8_lossy(&output).trim().to_string();
    if token.is_empty() {
        return Err("the ntfy token command printed nothing".into());
    }
    if token.chars().any(char::is_control) {
        return Err("the ntfy token must be a single line".into());
    }
    Ok(Config { base, topic: topic.to_string(), token })
}

/// The URL a publish posts to: the base URL, then the topic.
fn endpoint(config: &Config) -> String {
    format!("{}/{}", config.base, config.topic)
}

/// A header value ntfy reads as the title. ASCII passes through; anything else is RFC 2047
/// encoded (`=?UTF-8?B?...?=`), which keeps control characters and non-ASCII text out of the header.
fn header_text(text: &str) -> String {
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        text.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(text))
    }
}

/// POST `body` to the topic as one notification. `priority` is ntfy's 1 (min) to 5 (max); `tags` are
/// ntfy's emoji shortcodes. Blocking, with a 5 s timeout: call it off the request path.
pub fn publish(config: &Config, title: &str, body: &str, priority: u8, tags: &[&str]) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let mut request = agent
        .post(&endpoint(config))
        .set("Authorization", &format!("Bearer {}", config.token))
        .set("Title", &header_text(title))
        .set("Priority", &priority.to_string());
    if !tags.is_empty() {
        request = request.set("Tags", &tags.join(","));
    }
    match request.send_string(body) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => Err(format!("ntfy refused the push: HTTP {code}")),
        Err(ureq::Error::Transport(t)) => Err(format!("ntfy push failed: {t}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// A one-shot HTTP server on 127.0.0.1 that answers `status` and sends back the raw request
    /// (request line, headers and body) it received. Returns its base URL and the request channel.
    fn fake_ntfy(status: &'static str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut raw: Vec<u8> = Vec::new();
            let mut buf = [0u8; 1024];
            let head_end = loop {
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break raw.len();
                }
                raw.extend_from_slice(&buf[..n]);
            };
            let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
            let length = head
                .lines()
                .find_map(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while raw.len() < head_end + length {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }
            let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
            let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
        });
        (format!("http://{addr}"), rx)
    }

    fn config(base: String) -> Config {
        Config { base, topic: "agents".into(), token: "tok-test-123".into() }
    }

    #[test]
    fn a_publish_posts_to_the_topic_with_the_bearer_token_and_headers() {
        let (base, rx) = fake_ntfy("200 OK");
        publish(&config(base), "Agent finished", "fix the parser", 3, &["white_check_mark"]).unwrap();
        let request = rx.recv().unwrap();
        assert!(request.starts_with("POST /agents HTTP/1.1\r\n"), "request line: {request}");
        assert!(request.contains("Authorization: Bearer tok-test-123\r\n"));
        assert!(request.contains("Title: Agent finished\r\n"));
        assert!(request.contains("Priority: 3\r\n"));
        assert!(request.contains("Tags: white_check_mark\r\n"));
        assert!(request.ends_with("\r\n\r\nfix the parser"), "body: {request}");
    }

    #[test]
    fn a_refused_publish_names_the_status_and_not_the_token() {
        let (base, rx) = fake_ntfy("403 Forbidden");
        let err = publish(&config(base), "t", "b", 4, &[]).unwrap_err();
        assert_eq!(err, "ntfy refused the push: HTTP 403");
        assert!(!err.contains("tok-test-123"));
        let request = rx.recv().unwrap();
        assert!(!request.contains("Tags:"), "an empty tag list sends no Tags header");
    }

    #[test]
    fn a_non_ascii_title_is_rfc2047_encoded() {
        assert_eq!(header_text("plain title"), "plain title");
        assert_eq!(header_text("a\nb"), "=?UTF-8?B?YQpi?=");
        assert_eq!(header_text("é"), "=?UTF-8?B?w6k=?=");
    }

    #[test]
    fn the_endpoint_is_the_base_then_the_topic() {
        let c = Config { base: "http://ntfy.local:8080".into(), topic: "agents".into(), token: "x".into() };
        assert_eq!(endpoint(&c), "http://ntfy.local:8080/agents");
    }

    #[test]
    fn load_rejects_a_bad_url_or_topic_before_running_anything() {
        assert!(load("ftp://host", "agents", "echo x").is_err());
        assert!(load("http://", "agents", "echo x").is_err());
        assert!(load("http://host", "", "echo x").is_err());
        assert!(load("http://host", "a/b", "echo x").is_err());
        assert!(load("http://host", "a b", "echo x").is_err());
    }

    #[test]
    fn load_trims_the_token_and_strips_the_trailing_slash() {
        let c = load("http://host:30181/", "agents", "printf '  tok-abc \\n'").unwrap();
        assert_eq!(c.base, "http://host:30181");
        assert_eq!(c.token, "tok-abc");
    }

    #[test]
    fn a_failing_token_command_reports_its_status_without_the_token() {
        let err = load("http://host", "agents", "printf 'SECRET-XYZ'; exit 3").err().expect("the command fails");
        assert!(err.contains("token command failed"), "error: {err}");
        assert!(!err.contains("SECRET-XYZ"));
    }

    #[test]
    fn an_empty_token_is_an_error() {
        let err = load("http://host", "agents", "printf '  \\n'").err().expect("an empty token is refused");
        assert_eq!(err, "the ntfy token command printed nothing");
        assert!(load("http://host", "agents", "   ").is_err());
    }
}
