//! Phone dictation (`speech.*`). The app's mic records 16 kHz mono PCM and streams it to this bridge as
//! `speech.dictation.chunk` calls. `finish` wraps the recording as a WAV and posts it to the local whisper
//! server (llama-swap's OpenAI-compatible transcription route), the one `~/.local/bin/dictate-toggle.sh`
//! uses. The transcript comes back with the `[TRANSCRIBED] ` marker the script pastes, and the app inserts it
//! into the composer. Nothing is sent from here.
//!
//! One dictation is held at a time. Its audio stays in memory and is dropped on finish, on cancel, or when
//! the connection that started it closes. Client side: mobile/src/hooks/use-mobile-dictation.ts.

use std::io::ErrorKind as IoKind;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use parking_lot::Mutex;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};

use super::rpc::Failure;

/// The rate the app records at (`MOBILE_DICTATION_PCM_SAMPLE_RATE`).
pub const SAMPLE_RATE: u32 = 16_000;
const BYTES_PER_SAMPLE: usize = 2;
/// The longest chunk the app sends: five seconds of PCM (`MAX_DICTATION_AUDIO_CHUNK_BYTES`).
const MAX_CHUNK_BYTES: usize = 5 * SAMPLE_RATE as usize * BYTES_PER_SAMPLE;
/// The longest base64 text of one chunk (`MAX_DICTATION_AUDIO_CHUNK_BASE64_LENGTH`).
const MAX_CHUNK_BASE64_CHARS: usize = MAX_CHUNK_BYTES.div_ceil(3) * 4;
/// The longest recording the bridge holds: five minutes of PCM, about 9.6 MB.
const MAX_AUDIO_BYTES: usize = 5 * 60 * SAMPLE_RATE as usize * BYTES_PER_SAMPLE;
/// How long the whisper server may take, as the desktop script's `curl -m 60`.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(60);
/// The defaults `dictate-toggle.sh` uses.
const DEFAULT_URL: &str = "http://10.10.20.3:11435/v1/audio/transcriptions";
const DEFAULT_MODEL: &str = "whisper-gpu";
const DEFAULT_LANGUAGE: &str = "auto";
/// The marker the desktop script puts before every transcript, so the agent knows it was dictated.
pub const TRANSCRIBED_PREFIX: &str = "[TRANSCRIBED] ";

/// Where transcription goes and which model answers.
#[derive(Clone, Debug)]
pub struct Whisper {
    pub url: String,
    pub model: String,
    pub language: String,
    pub timeout: Duration,
}

impl Whisper {
    /// `SESYNC_WHISPER_URL`, `SESYNC_WHISPER_MODEL` and `SESYNC_WHISPER_LANGUAGE`, each defaulting to the
    /// value the desktop script uses.
    pub fn from_env() -> Whisper {
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Whisper {
            url: var("SESYNC_WHISPER_URL", DEFAULT_URL),
            model: var("SESYNC_WHISPER_MODEL", DEFAULT_MODEL),
            language: var("SESYNC_WHISPER_LANGUAGE", DEFAULT_LANGUAGE),
            timeout: TRANSCRIBE_TIMEOUT,
        }
    }
}

/// The dictation in progress, shared by every connection through `Ctx`.
#[derive(Clone)]
pub struct Dictation {
    session: Arc<Mutex<Option<Session>>>,
    whisper: Arc<Whisper>,
}

/// The dictation in progress: its id, the connection that started it, and the PCM received so far.
struct Session {
    id: String,
    conn: u64,
    audio: Vec<u8>,
}

impl Dictation {
    pub fn new(whisper: Whisper) -> Dictation {
        Dictation { session: Arc::new(Mutex::new(None)), whisper: Arc::new(whisper) }
    }

    /// `speech.models.list`: the one model, always ready. An unreachable server surfaces at `finish`.
    pub fn models(&self) -> Value {
        json!({
            "enabled": true,
            "selectedModelId": self.whisper.model,
            "dictationMode": "toggle",
            "models": [{
                "id": self.whisper.model,
                "label": self.whisper.model,
                "provider": "local",
                "status": "ready",
                "sizeBytes": null,
                "progress": null,
            }],
        })
    }

    /// `speech.dictation.start`: `{dictationId, modelId?}`. A dictation this connection already holds is
    /// refused. One held by another connection is replaced, since the phone that is speaking now is the one
    /// the operator is using, and the other's connection may be half-open.
    pub fn start(&self, params: &Value, conn: u64) -> Result<Value, Failure> {
        let id = dictation_id(params)?;
        if let Some(model) = params["modelId"].as_str() {
            if model != self.whisper.model {
                return Err(bad(&format!("Dictation model {model} is not available on this computer")));
            }
        }
        let mut session = self.session.lock();
        if session.as_ref().is_some_and(|s| s.conn == conn) {
            return Err(("dictation_already_active", "a dictation is already in progress".into()));
        }
        *session = Some(Session { id: id.clone(), conn, audio: Vec::new() });
        Ok(json!({ "dictationId": id, "modelId": self.whisper.model }))
    }

    /// `speech.dictation.chunk`: `{dictationId, audioBase64, sampleRate}`, one chunk of PCM s16le.
    pub fn chunk(&self, params: &Value, conn: u64) -> Result<Value, Failure> {
        let id = dictation_id(params)?;
        if params["sampleRate"].as_f64() != Some(f64::from(SAMPLE_RATE)) {
            return Err(bad("Dictation audio must be 16 kHz"));
        }
        let text = params["audioBase64"].as_str().filter(|t| !t.is_empty()).ok_or_else(|| bad("Missing audio chunk"))?;
        // Bound the text before decoding it, so an oversize chunk never reaches an allocation.
        if text.len() > MAX_CHUNK_BASE64_CHARS {
            return Err(bad("Dictation audio chunk is longer than 5 seconds"));
        }
        let pcm = STANDARD.decode(text).map_err(|_| bad("Dictation audio chunk must be base64"))?;
        if pcm.len() % BYTES_PER_SAMPLE != 0 {
            return Err(bad("Dictation audio must be 16-bit samples"));
        }
        let mut guard = self.session.lock();
        check_owner(guard.as_ref(), &id, conn)?;
        let session = guard.as_mut().expect("checked above");
        if session.audio.len() + pcm.len() > MAX_AUDIO_BYTES {
            return Err(bad("Dictation is longer than 5 minutes"));
        }
        session.audio.extend_from_slice(&pcm);
        Ok(json!({ "dictationId": id }))
    }

    /// `speech.dictation.finish`: `{dictationId, text}`. `text` is `[TRANSCRIBED] …`, or empty when the
    /// whisper server heard nothing. The session is taken before the request, so nothing holds the lock while
    /// whisper works.
    pub fn finish(&self, params: &Value, conn: u64) -> Result<Value, Failure> {
        let id = dictation_id(params)?;
        let session = {
            let mut guard = self.session.lock();
            check_owner(guard.as_ref(), &id, conn)?;
            guard.take()
        };
        let audio = session.map(|s| s.audio).unwrap_or_default();
        if audio.is_empty() {
            return Err(bad("No audio captured"));
        }
        let transcript = transcribe(&self.whisper, &wav(&audio))?;
        let text = if transcript.is_empty() { String::new() } else { format!("{TRANSCRIBED_PREFIX}{transcript}") };
        Ok(json!({ "dictationId": id, "text": text }))
    }

    /// `speech.dictation.cancel`: `{dictationId}`. Only the connection that started the dictation can cancel
    /// it. Any other call is a successful no-op, as the desktop runtime answers it.
    pub fn cancel(&self, params: &Value, conn: u64) -> Result<Value, Failure> {
        let id = dictation_id(params)?;
        let mut guard = self.session.lock();
        if guard.as_ref().is_some_and(|s| s.id == id && s.conn == conn) {
            *guard = None;
        }
        Ok(json!({ "dictationId": id }))
    }

    /// Drops the dictation a closed connection started, with its audio.
    pub fn cancel_connection(&self, conn: u64) {
        let mut guard = self.session.lock();
        if guard.as_ref().is_some_and(|s| s.conn == conn) {
            *guard = None;
        }
    }
}

fn bad(message: &str) -> Failure {
    ("bad_request", message.to_string())
}

fn unavailable(message: String) -> Failure {
    ("unavailable", message)
}

fn dictation_id(params: &Value) -> Result<String, Failure> {
    params["dictationId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| bad("Missing dictation ID"))
}

/// Refuses unless `conn` holds the dictation named `id`.
fn check_owner(session: Option<&Session>, id: &str, conn: u64) -> Result<(), Failure> {
    match session {
        Some(s) if s.id == id && s.conn == conn => Ok(()),
        Some(s) if s.id == id => Err(("dictation_owner_mismatch", "this dictation belongs to another phone".into())),
        _ => Err(("dictation_stream_not_started", "no dictation is in progress with that id".into())),
    }
}

/// A 16 kHz mono 16-bit PCM WAV around `pcm`.
fn wav(pcm: &[u8]) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = SAMPLE_RATE * BYTES_PER_SAMPLE as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

/// POST the WAV as the desktop script does (`file`, `temperature`, `model`, `language`,
/// `response_format`) and return the trimmed transcript.
fn transcribe(whisper: &Whisper, wav: &[u8]) -> Result<String, Failure> {
    let boundary = boundary();
    let body = multipart(&boundary, wav, whisper);
    let agent = ureq::AgentBuilder::new().timeout(whisper.timeout).build();
    let reply = agent
        .post(&whisper.url)
        .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
        .send_bytes(&body);
    let response = match reply {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            return Err(unavailable(format!("the speech server answered HTTP {code}; it may be busy")))
        }
        Err(ureq::Error::Transport(transport)) if timed_out(&transport) => {
            return Err(unavailable(format!(
                "transcription timed out after {} s; the GPU may be busy with another model",
                whisper.timeout.as_secs()
            )))
        }
        Err(ureq::Error::Transport(transport)) => {
            return Err(unavailable(format!("the speech server is unreachable: {transport}")))
        }
    };
    let text = response
        .into_string()
        .map_err(|e| unavailable(format!("the speech server's reply could not be read: {e}")))?;
    let reply: Value = serde_json::from_str(&text)
        .map_err(|_| unavailable("the speech server's reply is not JSON".into()))?;
    let transcript = reply["text"]
        .as_str()
        .ok_or_else(|| unavailable("the speech server's reply has no text".into()))?;
    Ok(transcript.trim().to_string())
}

fn multipart(boundary: &str, wav: &[u8], whisper: &Whisper) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"dictate.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    let fields = [
        ("temperature", "0.0"),
        ("model", whisper.model.as_str()),
        ("language", whisper.language.as_str()),
        ("response_format", "json"),
    ];
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn boundary() -> String {
    let mut random = [0u8; 12];
    SystemRandom::new().fill(&mut random).expect("the system random source is available");
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    format!("sesync{hex}")
}

/// A read that gave up at the deadline, as opposed to a refused or reset connection.
fn timed_out(transport: &ureq::Transport) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(transport);
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            return matches!(io.kind(), IoKind::TimedOut | IoKind::WouldBlock);
        }
        source = error.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// The fake whisper server's answer: status, JSON body, and how long it waits before answering.
    #[derive(Clone)]
    struct Reply {
        status: u16,
        body: &'static str,
        delay: Duration,
    }

    /// A one-request-at-a-time HTTP server standing in for whisper. It records every request body.
    struct Fake {
        url: String,
        bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    fn fake(reply: Reply) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/audio/transcriptions", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let body = read_request(&mut stream);
                seen.lock().push(body);
                std::thread::sleep(reply.delay);
                let head = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.body.as_bytes());
            }
        });
        Fake { url, bodies }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = stream.read(&mut buf).unwrap_or(0);
            if n == 0 {
                return data;
            }
            data.extend_from_slice(&buf[..n]);
            if let Some(head_end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..head_end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                    .unwrap_or(0);
                if data.len() >= head_end + 4 + length {
                    return data;
                }
            }
        }
    }

    fn whisper_at(url: &str, timeout: Duration) -> Whisper {
        Whisper { url: url.into(), model: "whisper-gpu".into(), language: "auto".into(), timeout }
    }

    fn dictation_with(url: &str) -> Dictation {
        Dictation::new(whisper_at(url, Duration::from_secs(5)))
    }

    /// Sample audio for the fake server, which never listens to it: a sawtooth `samples` long.
    fn pcm(samples: usize) -> Vec<u8> {
        (0..samples).flat_map(|i| ((i % 64) as i16 - 32).to_le_bytes()).collect()
    }

    fn chunk_params(id: &str, bytes: &[u8]) -> Value {
        json!({ "dictationId": id, "audioBase64": STANDARD.encode(bytes), "sampleRate": 16000 })
    }

    fn start_params(id: &str) -> Value {
        json!({ "dictationId": id })
    }

    fn dead_url() -> String {
        // Bind and drop, so the port refuses connections.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("http://127.0.0.1:{port}/v1/audio/transcriptions")
    }

    fn ok_text(text: &'static str) -> Reply {
        Reply { status: 200, body: text, delay: Duration::ZERO }
    }

    #[test]
    fn wav_header_describes_16k_mono_pcm16() {
        let out = wav(&[1, 2, 3, 4]);
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()), 40);
        assert_eq!(&out[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(out[20..22].try_into().unwrap()), 1, "PCM");
        assert_eq!(u16::from_le_bytes(out[22..24].try_into().unwrap()), 1, "mono");
        assert_eq!(u32::from_le_bytes(out[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(out[28..32].try_into().unwrap()), 32_000, "byte rate");
        assert_eq!(u16::from_le_bytes(out[32..34].try_into().unwrap()), 2, "block align");
        assert_eq!(u16::from_le_bytes(out[34..36].try_into().unwrap()), 16, "bits");
        assert_eq!(&out[36..40], b"data");
        assert_eq!(u32::from_le_bytes(out[40..44].try_into().unwrap()), 4);
        assert_eq!(&out[44..], &[1, 2, 3, 4]);
    }

    #[test]
    fn models_lists_the_configured_model_as_ready() {
        let d = dictation_with(&dead_url());
        let m = d.models();
        assert_eq!(m["selectedModelId"], "whisper-gpu");
        assert_eq!(m["dictationMode"], "toggle");
        assert_eq!(m["enabled"], true);
        assert_eq!(m["models"][0]["id"], "whisper-gpu");
        assert_eq!(m["models"][0]["status"], "ready");
    }

    #[test]
    fn finish_success_returns_the_marked_transcript_and_posts_the_wav() {
        let server = fake(ok_text(r#"{"text":" hello from the fake whisper "}"#));
        let d = dictation_with(&server.url);
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let reply = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap();
        assert_eq!(reply["dictationId"], "d1");
        assert_eq!(reply["text"], "[TRANSCRIBED] hello from the fake whisper");

        let bodies = server.bodies.lock();
        assert_eq!(bodies.len(), 1);
        let body = String::from_utf8_lossy(&bodies[0]);
        assert!(body.contains("name=\"model\"\r\n\r\nwhisper-gpu"), "{body}");
        assert!(body.contains("name=\"language\"\r\n\r\nauto"), "{body}");
        assert!(body.contains("name=\"temperature\"\r\n\r\n0.0"), "{body}");
        assert!(body.contains("name=\"response_format\"\r\n\r\njson"), "{body}");
        assert!(body.contains("filename=\"dictate.wav\""), "{body}");
        assert!(bodies[0].windows(4).any(|w| w == b"RIFF"), "the file part is a WAV");
    }

    #[test]
    fn finish_with_nothing_heard_returns_empty_text() {
        let server = fake(ok_text(r#"{"text":"   "}"#));
        let d = dictation_with(&server.url);
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let reply = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap();
        assert_eq!(reply["text"], "");
    }

    #[test]
    fn finish_without_any_audio_says_no_audio_was_captured() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        let err = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap_err();
        assert_eq!(err, ("bad_request", "No audio captured".to_string()));
    }

    #[test]
    fn a_server_error_status_is_reported_with_its_code() {
        let server = fake(Reply { status: 503, body: "{}", delay: Duration::ZERO });
        let d = dictation_with(&server.url);
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let err = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert!(err.1.contains("HTTP 503"), "{}", err.1);
    }

    #[test]
    fn a_reply_without_text_is_reported() {
        let server = fake(ok_text(r#"{"error":"model not loaded"}"#));
        let d = dictation_with(&server.url);
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let err = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert!(err.1.contains("has no text"), "{}", err.1);
    }

    #[test]
    fn a_reply_that_is_not_json_is_reported() {
        let server = fake(ok_text("<html>busy</html>"));
        let d = dictation_with(&server.url);
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let err = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert!(err.1.contains("not JSON"), "{}", err.1);
    }

    #[test]
    fn a_slow_server_times_out() {
        let server = fake(Reply { status: 200, body: r#"{"text":"late"}"#, delay: Duration::from_secs(3) });
        let d = Dictation::new(whisper_at(&server.url, Duration::from_millis(300)));
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let err = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert!(err.1.contains("timed out"), "{}", err.1);
    }

    #[test]
    fn an_unreachable_server_is_reported() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        let err = d.finish(&json!({ "dictationId": "d1" }), 1).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert!(err.1.contains("unreachable"), "{}", err.1);
    }

    #[test]
    fn a_finished_dictation_is_gone() {
        let server = fake(ok_text(r#"{"text":"once"}"#));
        let d = dictation_with(&server.url);
        d.start(&start_params("d1"), 1).unwrap();
        d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap();
        d.finish(&json!({ "dictationId": "d1" }), 1).unwrap();
        let err = d.chunk(&chunk_params("d1", &pcm(1600)), 1).unwrap_err();
        assert_eq!(err.1, "no dictation is in progress with that id");
    }

    #[test]
    fn start_refuses_a_second_dictation_on_the_same_connection() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        let err = d.start(&start_params("d2"), 1).unwrap_err();
        assert_eq!(err.0, "dictation_already_active");
    }

    #[test]
    fn start_replaces_a_dictation_another_connection_holds() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        d.start(&start_params("d2"), 2).unwrap();
        let err = d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap_err();
        assert_eq!(err.0, "dictation_stream_not_started");
    }

    #[test]
    fn start_refuses_a_model_this_bridge_does_not_serve() {
        let d = dictation_with(&dead_url());
        let err = d.start(&json!({ "dictationId": "d1", "modelId": "whisper-small" }), 1).unwrap_err();
        assert_eq!(err.0, "bad_request");
        d.start(&json!({ "dictationId": "d1", "modelId": "whisper-gpu" }), 1).unwrap();
    }

    #[test]
    fn a_chunk_before_start_is_refused() {
        let d = dictation_with(&dead_url());
        let err = d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap_err();
        assert_eq!(err.0, "dictation_stream_not_started");
    }

    #[test]
    fn a_chunk_from_another_connection_is_refused() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        let err = d.chunk(&chunk_params("d1", &pcm(16)), 2).unwrap_err();
        assert_eq!(err.0, "dictation_owner_mismatch");
    }

    #[test]
    fn audio_must_be_16khz_16bit_base64() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        let wrong_rate = json!({ "dictationId": "d1", "audioBase64": STANDARD.encode(pcm(16)), "sampleRate": 44100 });
        assert_eq!(d.chunk(&wrong_rate, 1).unwrap_err().1, "Dictation audio must be 16 kHz");
        let odd = chunk_params("d1", &[1, 2, 3]);
        assert_eq!(d.chunk(&odd, 1).unwrap_err().1, "Dictation audio must be 16-bit samples");
        let not_base64 = json!({ "dictationId": "d1", "audioBase64": "!!!!", "sampleRate": 16000 });
        assert_eq!(d.chunk(&not_base64, 1).unwrap_err().1, "Dictation audio chunk must be base64");
    }

    #[test]
    fn a_chunk_longer_than_five_seconds_is_refused() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        // Five seconds is accepted; one more sample is not.
        d.chunk(&chunk_params("d1", &pcm(SAMPLE_RATE as usize * 5)), 1).unwrap();
        let too_long = json!({
            "dictationId": "d1",
            "audioBase64": "A".repeat(MAX_CHUNK_BASE64_CHARS + 4),
            "sampleRate": 16000
        });
        assert_eq!(d.chunk(&too_long, 1).unwrap_err().1, "Dictation audio chunk is longer than 5 seconds");
    }

    #[test]
    fn a_recording_longer_than_five_minutes_is_refused() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        let five_seconds = pcm(SAMPLE_RATE as usize * 5);
        for _ in 0..(5 * 60 / 5) {
            d.chunk(&chunk_params("d1", &five_seconds), 1).unwrap();
        }
        let err = d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap_err();
        assert_eq!(err, ("bad_request", "Dictation is longer than 5 minutes".to_string()));
    }

    #[test]
    fn cancel_by_its_own_connection_drops_the_dictation() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        d.cancel(&json!({ "dictationId": "d1" }), 1).unwrap();
        assert_eq!(d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap_err().0, "dictation_stream_not_started");
    }

    #[test]
    fn cancel_by_another_connection_is_a_no_op() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        let reply = d.cancel(&json!({ "dictationId": "d1" }), 2).unwrap();
        assert_eq!(reply["dictationId"], "d1");
        d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap();
    }

    #[test]
    fn a_closed_connection_takes_its_dictation_with_it() {
        let d = dictation_with(&dead_url());
        d.start(&start_params("d1"), 1).unwrap();
        d.cancel_connection(2);
        d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap();
        d.cancel_connection(1);
        assert_eq!(d.chunk(&chunk_params("d1", &pcm(16)), 1).unwrap_err().0, "dictation_stream_not_started");
    }

    /// The PCM of a WAV's `data` chunk, found by walking the RIFF chunks (encoders add others, such as LIST).
    fn data_chunk(wav: &[u8]) -> Option<&[u8]> {
        let mut at = 12;
        while at + 8 <= wav.len() {
            let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().ok()?) as usize;
            let body = wav.get(at + 8..at + 8 + size)?;
            if &wav[at..at + 4] == b"data" {
                return Some(body);
            }
            at += 8 + size + size % 2;
        }
        None
    }

    /// Live smoke: the WAV's audio goes through start, chunk and finish against the real whisper server
    /// (`SESYNC_WHISPER_URL` or the default). Run by hand, once:
    /// `SESYNC_LIVE_WAV=/path/16k-mono.wav cargo test live_whisper -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the live whisper server and SESYNC_LIVE_WAV"]
    fn live_whisper_transcribes_a_wav_through_the_dictation_path() {
        let path = std::env::var("SESYNC_LIVE_WAV").expect("set SESYNC_LIVE_WAV to a 16 kHz mono WAV");
        let bytes = std::fs::read(path).unwrap();
        let pcm = data_chunk(&bytes).expect("the WAV has a data chunk");
        let d = Dictation::new(Whisper::from_env());
        d.start(&start_params("live"), 1).unwrap();
        for piece in pcm.chunks(MAX_CHUNK_BYTES) {
            d.chunk(&chunk_params("live", piece), 1).unwrap();
        }
        let reply = d.finish(&json!({ "dictationId": "live" }), 1).unwrap_or_else(|e| panic!("{}: {}", e.0, e.1));
        let text = reply["text"].as_str().unwrap_or_default();
        println!("LIVE_TRANSCRIPT: {text}");
        assert!(text.starts_with(TRANSCRIBED_PREFIX), "{reply}");
    }
}
