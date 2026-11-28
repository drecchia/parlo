//! `zapfast --mcp` as a real process: standard output carries only protocol
//! messages, and closing standard input stops it. The profile folders are
//! empty and temporary, so no account exists and nothing connects to WhatsApp.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn stdout_carries_only_protocol_messages_and_closing_stdin_stops_the_server() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_zapfast"))
        .arg("--mcp")
        .arg("--verbose")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("XDG_RUNTIME_DIR", home.path().join("run"))
        .env("APPDATA", home.path().join("appdata"))
        .env("LOCALAPPDATA", home.path().join("localappdata"))
        .env_remove("ZAPFAST_MCP_ALLOW_WRITE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("zapfast starts");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = child.stderr.take().unwrap();
    let logs =
        std::thread::spawn(move || BufReader::new(stderr).lines().map_while(Result::ok).count());

    let mut read = move || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "the server closed its output early");
        let message: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout held a non-protocol line ({error}): {line}"));
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
        message
    };
    let mut send = |message: serde_json::Value| {
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    };

    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}
    }));
    assert_eq!(read()["result"]["serverInfo"]["name"], "zapfast-mcp");
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "whatsapp_list_chats", "arguments": {}}
    }));
    let answer = read();
    assert_eq!(answer["result"]["isError"], true);
    assert_eq!(
        answer["result"]["structuredContent"]["error_code"],
        "not_linked"
    );

    // Closing standard input is how a client says it is done.
    drop(stdin);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the server kept running after its input closed"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
    // The verbose log went to standard error, not into the protocol.
    assert!(logs.join().unwrap() > 0);
}
