//! External tools: places the Forward dialog can send selected messages to.
//!
//! A tool is a webhook (an HTTP endpoint) or an Orca bot. A person picks one
//! by hand for each send; a webhook's answer may come back as a draft in the
//! message box, never as a sent message. Nothing here logs or reports the
//! target (a webhook URL carries its key), the text, a file path, or a body.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize};

/// How long a tool may take to take the messages and answer.
const ANSWER_TIME: Duration = Duration::from_secs(300);
/// The longest answer kept, in bytes.
const MAX_ANSWER: usize = 64 * 1024;
/// How long one `orca` command may run.
const ORCA_TIME: Duration = Duration::from_secs(30);
/// The most of an `orca` command's output read, in bytes.
const ORCA_OUTPUT: u64 = 1024 * 1024;

/// What a tool is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// An HTTP endpoint: a POST with the text, and the media as parts.
    #[default]
    Webhook,
    /// A bot of the Orca app on this computer, reached through its CLI.
    Orca,
}

impl ToolKind {
    pub const ALL: [ToolKind; 2] = [Self::Webhook, Self::Orca];

    pub fn label(self) -> &'static str {
        match self {
            Self::Webhook => "Webhook",
            Self::Orca => "Orca bot",
        }
    }
}

/// One tool from Settings.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tool {
    pub name: String,
    pub kind: ToolKind,
    /// Webhook: the URL. Orca: the bot's name.
    pub target: String,
    /// Whether a webhook's answer becomes a draft in the origin chat.
    pub answer: bool,
}

impl Default for Tool {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: ToolKind::Webhook,
            target: String::new(),
            answer: true,
        }
    }
}

/// Leaves the target out: a webhook URL carries its key.
impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Tool {
    /// Whether the Forward dialog offers it.
    pub fn usable(&self) -> bool {
        !self.name.trim().is_empty() && !self.target.trim().is_empty()
    }
}

/// Reads the Settings list, dropping an entry this build does not
/// understand rather than failing the whole settings file.
pub fn tolerant<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Tool>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Array(entries) = value else {
        return Ok(Vec::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect())
}

/// A downloaded media file sent with the text.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolFile {
    pub path: PathBuf,
    pub mime: String,
    /// The name the tool sees, such as `1.jpg`.
    pub name: String,
}

/// Sends the text and files to the tool, with the person's prompt when there
/// is one. `Ok(Some(text))` is its answer, `Ok(None)` a delivery without one,
/// `Err` a sentence for a toast.
pub fn send(
    tool: &Tool,
    text: &str,
    prompt: Option<&str>,
    files: &[ToolFile],
) -> Result<Option<String>, String> {
    match tool.kind {
        ToolKind::Webhook => send_webhook(tool, text, prompt, files),
        ToolKind::Orca => send_orca(tool, text, prompt, files),
    }
}

fn send_webhook(
    tool: &Tool,
    text: &str,
    prompt: Option<&str>,
    files: &[ToolFile],
) -> Result<Option<String>, String> {
    use reqwest::blocking::multipart::{Form, Part};
    let name = &tool.name;
    let refused = || "A webhook must use HTTPS, or HTTP on this computer".to_owned();
    let url = reqwest::Url::parse(tool.target.trim()).map_err(|_| refused())?;
    let local = is_loopback(&url);
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        _ => return Err(refused()),
    }
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(concat!("ZapFast/", env!("CARGO_PKG_VERSION")))
        .timeout(ANSWER_TIME)
        .redirect(reqwest::redirect::Policy::none());
    if local {
        builder = builder.no_proxy();
    } else if let Some(proxy) = crate::proxy::reqwest_proxy() {
        builder = builder.proxy(proxy);
    }
    let unreachable = || format!("{name} could not be reached");
    let client = builder.build().map_err(|_| unreachable())?;
    let request = client.post(url);
    let request = if files.is_empty() {
        // Without a prompt the body is exactly what it was before prompts.
        let mut body = serde_json::json!({ "text": text });
        if let Some(prompt) = prompt {
            body["prompt"] = prompt.into();
        }
        request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
    } else {
        let mut form = Form::new().text("text", text.to_owned());
        if let Some(prompt) = prompt {
            form = form.text("prompt", prompt.to_owned());
        }
        for file in files {
            let bytes = std::fs::read(&file.path)
                .map_err(|_| format!("Could not read the media for {name}"))?;
            let mime = if Part::bytes(Vec::new()).mime_str(&file.mime).is_ok() {
                file.mime.as_str()
            } else {
                "application/octet-stream"
            };
            let part = Part::bytes(bytes)
                .file_name(file.name.clone())
                .mime_str(mime)
                .map_err(|_| format!("Could not read the media for {name}"))?;
            form = form.part("file", part);
        }
        request.multipart(form)
    };
    let response = request.send().map_err(|_| unreachable())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{name} answered with HTTP {}", status.as_u16()));
    }
    if !tool.answer {
        return Ok(None);
    }
    let json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    let mut body = Vec::new();
    response
        .take(MAX_ANSWER as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| format!("{name}'s answer could not be read"))?;
    if body.len() > MAX_ANSWER {
        return Err(format!("{name}'s answer is too long"));
    }
    let answer = if json {
        serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("text")?.as_str().map(str::to_owned))
            .unwrap_or_default()
    } else {
        String::from_utf8_lossy(&body).into_owned()
    };
    let answer = answer.trim();
    Ok((!answer.is_empty()).then(|| answer.to_owned()))
}

/// Whether the URL names this computer.
fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Hands the messages to an Orca bot as a question in a new chat of its
/// own. Each send gets its own Orca Run, whose inbox receives the bot's
/// answer: Orca mails answers to the asker, and ZapFast has no mailbox of
/// its own. Media goes as paths on this computer, which the bot can open.
fn send_orca(
    tool: &Tool,
    text: &str,
    prompt: Option<&str>,
    files: &[ToolFile],
) -> Result<Option<String>, String> {
    let bot = tool.target.trim();
    if bot.is_empty()
        || !bot
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("An Orca bot name has only letters, digits, - and _".to_owned());
    }
    let name = &tool.name;
    let missing = || "Orca was not found on this computer".to_owned();
    let refused = || format!("Orca refused the message for {name}");
    let objective = format!("ZapFast to {}", name.trim());
    let run = orca(
        &[
            "orchestration".as_ref(),
            "run-create".as_ref(),
            "--objective".as_ref(),
            objective.as_ref(),
            "--json".as_ref(),
        ],
        ORCA_TIME,
    )
    .map_err(|()| missing())?
    .and_then(|value| Some(value.get("result")?.get("runId")?.as_str()?.to_owned()))
    .ok_or_else(refused)?;
    let mut body = tempfile::Builder::new()
        .prefix("zapfast-tool-")
        .suffix(".txt")
        .tempfile()
        .map_err(|_| refused())?;
    std::io::Write::write_all(&mut body, orca_body(text, prompt, files).as_bytes())
        .map_err(|_| refused())?;
    let to = format!("bot:{bot}");
    orca(
        &[
            "orchestration".as_ref(),
            "send".as_ref(),
            "--run".as_ref(),
            run.as_ref(),
            "--to".as_ref(),
            to.as_ref(),
            "--type".as_ref(),
            "question".as_ref(),
            "--chat".as_ref(),
            "new".as_ref(),
            "--subject".as_ref(),
            "From ZapFast".as_ref(),
            "--body-file".as_ref(),
            body.path().as_os_str(),
            "--json".as_ref(),
        ],
        ORCA_TIME,
    )
    .map_err(|()| missing())?
    .ok_or_else(refused)?;
    if !tool.answer {
        return Ok(None);
    }
    let deadline = Instant::now() + ANSWER_TIME;
    // Orca may first mail a note that the bot's turn ended; the answer
    // itself comes after it.
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let wait = left.as_millis().max(1).to_string();
        match orca(
            &[
                "orchestration".as_ref(),
                "check".as_ref(),
                "--run".as_ref(),
                run.as_ref(),
                "--wait".as_ref(),
                "--timeout-ms".as_ref(),
                wait.as_ref(),
                "--json".as_ref(),
            ],
            left + ORCA_TIME,
        ) {
            Ok(Some(mail)) => {
                if let Some(answer) = orca_answer(&mail) {
                    return Ok(Some(answer));
                }
            }
            // A wait that ran out may end as a failure.
            Ok(None) if Instant::now() >= deadline => break,
            _ => return Err(refused()),
        }
    }
    Err(format!(
        "{name} did not answer in time. Its answer is in Orca."
    ))
}

/// The bot's answer among a Run's mail: the body of the first message
/// carrying Orca's `botAnswer`, cut to [`MAX_ANSWER`].
fn orca_answer(mail: &serde_json::Value) -> Option<String> {
    let answer = mail
        .get("result")?
        .get("messages")?
        .as_array()?
        .iter()
        .filter(|message| {
            message
                .get("payload")
                .and_then(|p| p.get("botAnswer"))
                .is_some()
        })
        .find_map(|message| message.get("body")?.as_str())?;
    let mut end = answer.len().min(MAX_ANSWER);
    while !answer.is_char_boundary(end) {
        end -= 1;
    }
    let answer = answer[..end].trim();
    (!answer.is_empty()).then(|| answer.to_owned())
}

/// The message an Orca bot reads: the person's prompt when there is one, the
/// messages, then where the media is. Without a prompt it is the messages
/// alone, as before prompts.
fn orca_body(text: &str, prompt: Option<&str>, files: &[ToolFile]) -> String {
    let mut body = match prompt {
        Some(prompt) => format!("Prompt:\n{prompt}\n\nForwarded messages:\n{text}"),
        None => text.to_owned(),
    };
    if !files.is_empty() {
        body.push_str("\n\nFiles on this computer:\n");
        for file in files {
            body.push_str(&format!("- {}\n", file.path.display()));
        }
    }
    body
}

/// The Orca CLI, as Orca's own guide resolves it. On Linux a bare `orca` is
/// usually the GNOME screen reader.
fn orca_program() -> OsString {
    if let Some(command) = std::env::var_os("ORCA_CLI_COMMAND").filter(|c| !c.is_empty()) {
        return command;
    }
    if cfg!(target_os = "linux") {
        "orca-ide".into()
    } else {
        "orca".into()
    }
}

/// Runs the Orca CLI for at most `time`: its JSON when it reports
/// `"ok": true`, `None` for any other end, `Err` when it cannot start.
fn orca(args: &[&OsStr], time: Duration) -> Result<Option<serde_json::Value>, ()> {
    let mut command = Command::new(orca_program());
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|_| ())?;
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.take(ORCA_OUTPUT).read_to_end(&mut out);
        }
        out
    });
    let deadline = Instant::now() + time;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().unwrap_or_default();
    if !status.is_some_and(|status| status.success()) {
        return Ok(None);
    }
    Ok(reported_ok(&out))
}

/// The CLI's JSON when it says `"ok": true`.
fn reported_ok(out: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice::<serde_json::Value>(out)
        .ok()
        .filter(|value| value.get("ok") == Some(&serde_json::Value::Bool(true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// Serves one request and hands back what arrived.
    fn serve(
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook?key=secret", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                if read == 0 {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    fn webhook(url: &str) -> Tool {
        Tool {
            name: "Echo".into(),
            kind: ToolKind::Webhook,
            target: url.into(),
            answer: true,
        }
    }

    #[test]
    fn a_webhook_gets_json_and_its_text_field_becomes_the_answer() {
        let (url, server) = serve("200 OK", "application/json", r#"{"text":" pong "}"#);
        let answer = send(&webhook(&url), "hi", None, &[]);
        let request = server.join().unwrap();
        assert_eq!(answer, Ok(Some("pong".to_owned())));
        assert!(
            request
                .to_lowercase()
                .contains("content-type: application/json")
        );
        // Without a prompt, exactly the body sent before prompts existed.
        assert!(request.ends_with(r#"{"text":"hi"}"#));
    }

    #[test]
    fn a_prompt_goes_in_its_own_json_field() {
        let (url, server) = serve("200 OK", "text/plain", "");
        assert_eq!(
            send(&webhook(&url), "hi", Some("Summarize this"), &[]),
            Ok(None)
        );
        let request = server.join().unwrap();
        let body = &request[request.find("\r\n\r\n").unwrap() + 4..];
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "text": "hi", "prompt": "Summarize this" })
        );
    }

    #[test]
    fn media_goes_as_multipart_parts_with_the_prompt_only_when_given() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"jpeg bytes").unwrap();
        let files = [ToolFile {
            path: file.path().to_owned(),
            mime: "image/jpeg".into(),
            name: "1.jpg".into(),
        }];
        for prompt in [Some("Transcribe it"), None] {
            let (url, server) = serve("200 OK", "text/plain", "");
            assert_eq!(send(&webhook(&url), "hi", prompt, &files), Ok(None));
            let request = server.join().unwrap();
            assert!(request.contains("name=\"text\"\r\n\r\nhi\r\n"));
            assert!(request.contains(r#"name="file"; filename="1.jpg""#));
            assert!(request.contains("image/jpeg"));
            assert!(request.contains("jpeg bytes"));
            assert_eq!(
                request.contains("name=\"prompt\"\r\n\r\nTranscribe it\r\n"),
                prompt.is_some()
            );
            assert_eq!(request.contains("name=\"prompt\""), prompt.is_some());
        }
    }

    #[test]
    fn an_answer_without_text_is_none() {
        let (url, server) = serve("200 OK", "application/json", r#"{"ok":true}"#);
        assert_eq!(send(&webhook(&url), "hi", None, &[]), Ok(None));
        server.join().unwrap();
        let (url, server) = serve("200 OK", "text/plain", "  ");
        assert_eq!(send(&webhook(&url), "hi", None, &[]), Ok(None));
        server.join().unwrap();
        let (url, server) = serve("200 OK", "text/plain", "pong");
        assert_eq!(
            send(&webhook(&url), "hi", None, &[]),
            Ok(Some("pong".to_owned()))
        );
        server.join().unwrap();
    }

    #[test]
    fn answer_off_ignores_the_body() {
        let (url, server) = serve("200 OK", "text/plain", "pong");
        let tool = Tool {
            answer: false,
            ..webhook(&url)
        };
        assert_eq!(send(&tool, "hi", None, &[]), Ok(None));
        server.join().unwrap();
    }

    #[test]
    fn an_http_error_names_only_the_status() {
        let (url, server) = serve("500 Internal Server Error", "text/plain", "secret detail");
        let error = send(&webhook(&url), "hi", None, &[]).unwrap_err();
        server.join().unwrap();
        assert_eq!(error, "Echo answered with HTTP 500");
    }

    #[test]
    fn plain_http_off_this_computer_is_refused() {
        let error = send(&webhook("http://example.com/hook"), "hi", None, &[]).unwrap_err();
        assert_eq!(error, "A webhook must use HTTPS, or HTTP on this computer");
        assert!(send(&webhook("ftp://localhost/"), "hi", None, &[]).is_err());
    }

    #[test]
    fn debug_hides_the_target() {
        let shown = format!("{:?}", webhook("https://example.com/hook?key=secret"));
        assert!(shown.contains("Echo"));
        assert!(!shown.contains("secret"));
    }

    /// Asks a real Orca bot, named by `ZAPFAST_ORCA_BOT`, through the Orca
    /// CLI on this computer, and expects its answer back.
    #[test]
    #[ignore = "asks a real Orca bot; set ZAPFAST_ORCA_BOT"]
    fn a_real_orca_bot_answers() {
        let Some(bot) = std::env::var("ZAPFAST_ORCA_BOT").ok() else {
            return;
        };
        let tool = Tool {
            name: "Orca".into(),
            kind: ToolKind::Orca,
            target: bot,
            answer: true,
        };
        let answer = send(&tool, "Reply with the single word pong", None, &[]);
        assert!(
            answer.as_ref().is_ok_and(|answer| answer
                .as_deref()
                .is_some_and(|a| a.to_lowercase().contains("pong"))),
            "{answer:?}"
        );
    }

    #[test]
    fn an_orca_bot_name_cannot_carry_arguments() {
        let tool = Tool {
            kind: ToolKind::Orca,
            target: "bot --to other".into(),
            ..webhook("")
        };
        assert_eq!(
            send(&tool, "hi", None, &[]),
            Err("An Orca bot name has only letters, digits, - and _".to_owned())
        );
    }

    #[test]
    fn an_orca_body_lists_the_media_paths() {
        let files = [ToolFile {
            path: PathBuf::from("media").join("a.jpg"),
            mime: "image/jpeg".into(),
            name: "1.jpg".into(),
        }];
        let body = orca_body("hi", None, &files);
        assert!(body.starts_with("hi\n\nFiles on this computer:\n- "));
        assert!(body.ends_with("a.jpg\n"));
        assert_eq!(orca_body("hi", None, &[]), "hi");
    }

    #[test]
    fn an_orca_body_puts_the_prompt_before_the_messages() {
        assert_eq!(
            orca_body("[10:00] Ada: hi", Some("Open a ticket"), &[]),
            "Prompt:\nOpen a ticket\n\nForwarded messages:\n[10:00] Ada: hi"
        );
    }

    #[test]
    fn orca_success_needs_ok_true() {
        assert!(reported_ok(br#"{"ok":true,"result":{}}"#).is_some());
        assert!(reported_ok(br#"{"ok":false}"#).is_none());
        assert!(reported_ok(b"not json").is_none());
    }

    #[test]
    fn the_orca_answer_is_the_mail_with_a_bot_answer() {
        // Shaped like `orca orchestration check --run <id> --json`.
        let note = serde_json::json!({"ok": true, "result": {"messages": [
            {"type": "status", "subject": "Re: From ZapFast",
             "body": "BabyShark ended its turn without answering your question",
             "payload": {"requestId": "r"}},
        ]}});
        assert_eq!(orca_answer(&note), None);
        let answer = serde_json::json!({"ok": true, "result": {"messages": [
            {"type": "status", "subject": "Re: From ZapFast", "body": " pong ",
             "payload": {"requestId": "r", "botAnswer": {"from": "BabyShark"}}},
        ]}});
        assert_eq!(orca_answer(&answer), Some("pong".to_owned()));
        let long = serde_json::json!({"ok": true, "result": {"messages": [
            {"body": "é".repeat(MAX_ANSWER), "payload": {"botAnswer": {}}},
        ]}});
        assert!(orca_answer(&long).unwrap().len() <= MAX_ANSWER);
    }

    #[test]
    fn an_unknown_tool_is_dropped_and_the_rest_kept() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(deserialize_with = "tolerant")]
            tools: Vec<Tool>,
        }
        let holder: Holder = serde_json::from_str(
            r#"{"tools":[{"name":"A","kind":"carrier-pigeon","target":"x"},{"name":"B","kind":"orca","target":"bot"}]}"#,
        )
        .unwrap();
        assert_eq!(holder.tools.len(), 1);
        assert_eq!(holder.tools[0].name, "B");
        let holder: Holder = serde_json::from_str(r#"{"tools":"nonsense"}"#).unwrap();
        assert!(holder.tools.is_empty());
    }
}
