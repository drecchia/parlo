//! `zapfast --mcp` while the window is open: a relay between the client's
//! standard input and output and the window's private socket
//! ([`super::host`]). It reads no chat and holds no session; it adds the
//! handshake line and copies bytes both ways. Standard output carries only
//! what the window's MCP session wrote.

use std::path::Path;

use super::Options;

/// The variables the window may take into account for this client.
const FORWARDED: &[&str] = &[
    super::config::ALLOW_WRITE,
    super::config::ALLOW_SEND,
    super::config::ALLOW_FILES,
    super::config::FILE_ROOTS,
];

/// The handshake for this process's environment.
pub fn handshake(options: &Options, lookup: &impl Fn(&str) -> Option<String>) -> String {
    let env: std::collections::BTreeMap<_, _> = FORWARDED
        .iter()
        .filter_map(|name| lookup(name).map(|value| ((*name).to_owned(), value)))
        .collect();
    serde_json::json!({
        "zapfast_mcp": 1,
        "env": env,
        "account": options.accounts.first(),
    })
    .to_string()
}

#[cfg(unix)]
pub fn run(socket: &Path, options: &Options) -> anyhow::Result<()> {
    use anyhow::Context;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;
    runtime.block_on(async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| {
                "ZapFast is open but is not sharing with MCP clients. Turn on Settings, \
             \"Share with AI agents (MCP)\" and restart ZapFast, or close ZapFast to run \
             the MCP server on its own"
            })?;
        let (mut from_window, mut to_window) = stream.into_split();
        let line = handshake(options, &|name| std::env::var(name).ok());
        to_window.write_all(line.as_bytes()).await?;
        to_window.write_all(b"\n").await?;
        // Whatever the client writes goes to the window; closing it closes
        // the session, which ends the other direction below.
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buffer = [0u8; 8192];
            while let Ok(read) = stdin.read(&mut buffer).await {
                if read == 0 || to_window.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
            let _ = to_window.shutdown().await;
        });
        let mut stdout = tokio::io::stdout();
        let mut buffer = [0u8; 8192];
        loop {
            let read = from_window.read(&mut buffer).await?;
            if read == 0 {
                return Ok(());
            }
            stdout.write_all(&buffer[..read]).await?;
            stdout.flush().await?;
        }
    })
}

#[cfg(not(unix))]
pub fn run(_socket: &Path, _options: &Options) -> anyhow::Result<()> {
    anyhow::bail!(
        "ZapFast is already running. On this platform the MCP server cannot share it: close \
         ZapFast first"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_forwards_only_the_mcp_switches() {
        let lookup = |name: &str| match name {
            "ZAPFAST_MCP_ALLOW_WRITE" => Some("true".to_owned()),
            "ZAPFAST_MCP_ALLOW_APP_LOCKED" => Some("true".to_owned()),
            "HOME" => Some("/Users/x".to_owned()),
            _ => None,
        };
        let line = handshake(
            &Options {
                accounts: vec!["2".into()],
            },
            &lookup,
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["zapfast_mcp"], 1);
        assert_eq!(value["account"], "2");
        assert_eq!(value["env"]["ZAPFAST_MCP_ALLOW_WRITE"], "true");
        assert_eq!(value["env"].as_object().unwrap().len(), 1);
        assert!(!line.contains('\n'));
    }
}
