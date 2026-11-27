//! The MCP server inside the window.
//!
//! The window already owns every account's backend and the single-instance
//! lock, so a second process cannot open the session. Instead the window
//! listens on a private Unix socket (a file in the user's runtime directory,
//! mode 0600; no TCP port) and `zapfast --mcp` connects to it and relays its
//! standard input and output ([`super::proxy`]). Each connection is one
//! MCP session. The window passes every backend event to [`Host::feed`] and
//! the sessions send commands through clones of the backends' senders.
//!
//! What a client may do is the smaller of what it asked for in its
//! environment and the window's own settings: `mcp_allow_write` off keeps
//! every client read-only.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use super::bridge::AccountHandle;
use super::server::Registry;
use crate::backend::{Command, Event};

/// First line a client sends, before any MCP message.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Handshake {
    /// Protocol of this relay. Only 1 exists.
    pub zapfast_mcp: u32,
    /// The `ZAPFAST_MCP_*` variables the client was started with.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// The account to answer for when a tool names none.
    #[serde(default)]
    pub account: Option<String>,
}

/// Longest handshake line accepted.
pub const HANDSHAKE_LIMIT: u64 = 8192;

/// Where the window listens.
pub fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join("mcp.sock")
}

pub struct Host {
    registry: Registry,
    active: Arc<Mutex<String>>,
    socket: PathBuf,
    timeout: Duration,
    _runtime: tokio::runtime::Runtime,
}

impl Host {
    pub fn add_account(&self, id: &str, commands: UnboundedSender<Command>) {
        let handle = Arc::new(AccountHandle::shared(id, commands, self.timeout));
        self.registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_owned(), handle);
    }

    pub fn remove_account(&self, id: &str) {
        self.registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    /// Passes the window's account `id` one of its backend's events.
    pub fn feed(&self, id: &str, event: &Event) {
        let handle = self
            .registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned();
        if let Some(handle) = handle {
            handle.feed(event);
        }
    }

    /// The account on screen, which answers when a client names none.
    pub fn set_active(&self, id: &str) {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if *active != id {
            *active = id.to_owned();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::mcp::config::{Config, Permissions};
    use crate::mcp::server::{Accounts, ZapfastMcp};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    impl Host {
        /// Starts listening on `socket`. With `allow_write` false every
        /// client is read-only.
        pub fn start(
            socket: PathBuf,
            allow_write: bool,
            private_dirs: Vec<PathBuf>,
            active: String,
        ) -> std::io::Result<Self> {
            // A path too long for a socket address would fail to bind.
            if socket.as_os_str().len() >= 100 {
                return Err(std::io::Error::other(
                    "the runtime folder's path is too long for a socket",
                ));
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("zapfast-mcp-host")
                .enable_all()
                .build()?;
            // The single-instance lock is ours, so a socket left here by a
            // process that died belongs to no one.
            let _ = std::fs::remove_file(&socket);
            let listener = {
                let _inside = runtime.enter();
                UnixListener::bind(&socket)?
            };
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            let registry: Registry = Arc::default();
            let active = Arc::new(Mutex::new(active));
            let host = Self {
                registry: Arc::clone(&registry),
                active: Arc::clone(&active),
                socket,
                timeout: Config::default().timeout,
                _runtime: runtime,
            };
            let private_dirs = Arc::new(private_dirs);
            host._runtime.spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let registry = Arc::clone(&registry);
                    let active = Arc::clone(&active);
                    let private = Arc::clone(&private_dirs);
                    tokio::spawn(async move {
                        if let Err(error) =
                            session(stream, registry, active, allow_write, private).await
                        {
                            log::debug!("MCP client dropped: {error}");
                        }
                    });
                }
            });
            log::info!("MCP sharing is on");
            Ok(host)
        }
    }

    async fn session(
        stream: UnixStream,
        registry: Registry,
        active: Arc<Mutex<String>>,
        allow_write: bool,
        private: Arc<Vec<PathBuf>>,
    ) -> anyhow::Result<()> {
        let (read, write) = tokio::io::split(stream);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            (&mut read).take(HANDSHAKE_LIMIT).read_line(&mut line),
        )
        .await??;
        anyhow::ensure!(got > 0 && line.ends_with('\n'), "no handshake");
        let handshake: Handshake = serde_json::from_str(&line)?;
        anyhow::ensure!(handshake.zapfast_mcp == 1, "unknown relay version");
        let mut config = Config::from_lookup(&|name| handshake.env.get(name).cloned());
        if !allow_write {
            config.permissions = Permissions::read_only();
        }
        let default = handshake
            .account
            .filter(|account| !account.trim().is_empty())
            .unwrap_or_else(|| active.lock().unwrap_or_else(|p| p.into_inner()).clone());
        let accounts = Accounts::over(registry, default);
        let server = ZapfastMcp::new(Arc::new(accounts), Arc::new(config), (*private).clone());
        crate::mcp::serve(server, (read, write), std::future::pending()).await
    }
}

/// Starts the host for a window, when the settings and the platform allow it.
pub fn launch(
    runtime_dir: &Path,
    settings: &crate::settings::Settings,
    private_dirs: Vec<PathBuf>,
    active: &str,
) -> Option<Host> {
    if !settings.mcp_server {
        return None;
    }
    if settings.app_lock_hash.is_some() {
        log::warn!("MCP sharing stays off while the app lock is set");
        return None;
    }
    #[cfg(unix)]
    {
        match Host::start(
            socket_path(runtime_dir),
            settings.mcp_allow_write,
            private_dirs,
            active.to_owned(),
        ) {
            Ok(host) => Some(host),
            Err(error) => {
                log::warn!("MCP sharing could not start: {error}");
                None
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (runtime_dir, private_dirs, active);
        log::warn!("MCP sharing needs a Unix socket and is not available on this platform");
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::model::Chat;
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;

    fn host(
        dir: &Path,
        allow_write: bool,
    ) -> (Host, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let host = Host::start(socket_path(dir), allow_write, Vec::new(), "1".into()).unwrap();
        let (commands, inbox) = tokio::sync::mpsc::unbounded_channel();
        host.add_account("1", commands);
        host.feed("1", &Event::Link(crate::backend::LinkStatus::Connected));
        host.feed(
            "1",
            &Event::Chats(vec![Chat::new("1203@g.us".into(), "Team".into())]),
        );
        (host, inbox)
    }

    /// One MCP exchange over the socket, as the relay does it.
    fn call(
        dir: &Path,
        handshake: serde_json::Value,
        tool: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let mut stream = UnixStream::connect(socket_path(dir)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut send = |message: serde_json::Value| writeln!(stream, "{message}").unwrap();
        send(handshake);
        send(
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}}),
        );
        send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        send(
            serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}),
        );
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "closed early");
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            if message["id"] == 2 {
                return message["result"].clone();
            }
        }
    }

    #[test]
    fn the_window_answers_a_relayed_client_from_its_own_events() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, _inbox) = host(dir.path(), false);
        let result = call(
            dir.path(),
            serde_json::json!({"zapfast_mcp": 1, "env": {}}),
            "whatsapp_list_chats",
            serde_json::json!({}),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["chats"][0]["name"], "Team");
    }

    #[test]
    fn the_windows_setting_caps_what_a_client_asks_for() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, mut inbox) = host(dir.path(), false);
        let result = call(
            dir.path(),
            serde_json::json!({"zapfast_mcp": 1, "env": {"ZAPFAST_MCP_ALLOW_WRITE": "true"}}),
            "whatsapp_send_message",
            serde_json::json!({"chat_id": "1203@g.us", "text": "hi"}),
        );
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["error_code"],
            "permission_denied"
        );
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn a_client_may_send_when_the_window_allows_it() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, mut inbox) = host(dir.path(), true);
        // No worker answers, so the call would wait out its timeout; the
        // command reaching the backend is what matters.
        let path = dir.path().to_owned();
        std::thread::spawn(move || {
            call(
                &path,
                serde_json::json!({"zapfast_mcp": 1, "env": {"ZAPFAST_MCP_ALLOW_WRITE": "true"}}),
                "whatsapp_send_message",
                serde_json::json!({"chat_id": "1203@g.us", "text": "hi"}),
            )
        });
        let started = std::time::Instant::now();
        let command = loop {
            if let Ok(command) = inbox.try_recv() {
                break command;
            }
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "no command arrived"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(matches!(
            command,
            Command::SendText { chat, text, .. } if chat == "1203@g.us" && text == "hi"
        ));
    }

    #[test]
    fn the_socket_is_private_and_removed_with_the_host() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (host, _inbox) = host(dir.path(), false);
        let mode = std::fs::metadata(socket_path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(host);
        assert!(!socket_path(dir.path()).exists());
    }

    #[test]
    fn a_connection_without_a_handshake_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (_host, _inbox) = host(dir.path(), false);
        let mut stream = UnixStream::connect(socket_path(dir.path())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        writeln!(
            stream,
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}"
        )
        .unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
    }
}
