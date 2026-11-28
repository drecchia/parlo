//! `zapfast --mcp`: a Model Context Protocol server over stdio.
//!
//! The server is another interface to the same [`crate::backend::Backend`]
//! the window uses: it sends `Command`s and reads `Event`s, and reimplements
//! nothing of WhatsApp. It runs instead of the window, never beside it, so
//! one process owns the linked session. Standard output carries the protocol
//! and nothing else; logs go to standard error.
//!
//! Message text is written by other people. It is returned to the agent as
//! data, marked untrusted, and nothing in this module interprets it.

// Standard output belongs to the protocol. A stray print would corrupt it.
#![deny(clippy::print_stdout, clippy::dbg_macro)]

pub mod bridge;
pub mod config;
pub mod host;
pub mod phone;
pub mod proxy;
pub mod server;
pub mod types;

use std::sync::Arc;

use anyhow::{Context, bail};
use rmcp::ServiceExt;

use crate::backend::{Backend, Command, Waker};
use crate::model::AccountId;
use crate::paths::AppDirs;
use crate::settings::{AccountRoster, Settings};
use bridge::{AccountHandle, LinkState};
use config::Config;
use server::{Accounts, ZapfastMcp};

/// What `zapfast --mcp` was asked for on the command line.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Account ids to serve. Empty serves the account the window last used.
    pub accounts: Vec<String>,
}

/// Runs the server until the client closes standard input, or the process is
/// asked to stop. The caller holds the single-instance guard and has set up
/// logging and the directories, as for the window.
pub fn run(
    dirs: AppDirs,
    settings: Settings,
    options: Options,
    config: Config,
) -> anyhow::Result<()> {
    if settings.app_lock_hash.is_some() && !config.allow_app_locked {
        bail!(
            "The ZapFast app lock is set, and the MCP server would read chats without the \
             lock screen. If the lock should not apply to agents on this computer, start the \
             server with {}=true. Nothing was opened.",
            config::ALLOW_APP_LOCKED
        );
    }
    crate::proxy::configure(&settings.proxy);
    let roster =
        AccountRoster::load(&dirs.accounts_file()).context("could not read the accounts list")?;
    let ids = chosen_accounts(&roster, &options)?;
    let accounts = ids
        .iter()
        .map(|id| open_account(&dirs, id, &settings, &config))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let default = ids.first().expect("at least one account").to_string();
    let private = vec![dirs.state.clone(), dirs.config.clone()];
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("zapfast-mcp")
        .enable_all()
        .build()
        .context("could not start the async runtime")?;
    let result = runtime.block_on(async {
        let accounts = Arc::new(Accounts::new(default, accounts.clone()));
        let pumps: Vec<_> = accounts
            .all()
            .iter()
            .map(AccountHandle::spawn_pump)
            .collect();
        let server = ZapfastMcp::new(accounts, Arc::new(config), private);
        log::info!("MCP server ready on stdio");
        let served = serve(server, rmcp::transport::stdio(), shutdown_signal()).await;
        for pump in pumps {
            pump.abort();
        }
        served
    });
    // Stop every backend and wait for its thread, so the session and the
    // archive close cleanly, whichever way the server ended.
    let stops: Vec<_> = accounts
        .into_iter()
        .map(|account| std::thread::spawn(move || account.shutdown()))
        .collect();
    for stop in stops {
        let _ = stop.join();
    }
    log::info!("MCP server stopped");
    result
}

/// Serves `server` on `transport` until the client disconnects or `stop`
/// completes.
pub async fn serve<T, E, A>(
    server: ZapfastMcp,
    transport: T,
    stop: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let service = server
        .serve(transport)
        .await
        .map_err(|error| anyhow::anyhow!("could not start the MCP session: {error}"))?;
    let token = service.cancellation_token();
    let closed = service.waiting();
    tokio::pin!(closed);
    tokio::select! {
        _ = &mut closed => {}
        _ = stop => {
            token.cancel();
            let _ = closed.await;
        }
    }
    Ok(())
}

/// Completes on Ctrl-C or, where there is one, SIGTERM.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The accounts to serve: those asked for, else the one the window last used.
fn chosen_accounts(roster: &AccountRoster, options: &Options) -> anyhow::Result<Vec<AccountId>> {
    let order: Vec<&str> = if roster.order.is_empty() {
        vec!["1"]
    } else {
        roster.order.iter().map(String::as_str).collect()
    };
    let wanted: Vec<&str> = if options.accounts.is_empty() {
        let active = order
            .iter()
            .copied()
            .find(|id| *id == roster.active)
            .unwrap_or(order[0]);
        vec![active]
    } else {
        options.accounts.iter().map(String::as_str).collect()
    };
    let mut ids: Vec<AccountId> = Vec::new();
    for id in wanted {
        let Some(parsed) = AccountId::parse(id).filter(|_| order.contains(&id)) else {
            bail!(
                "There is no account {id:?}. This computer has: {}.",
                order.join(", ")
            );
        };
        if !ids.contains(&parsed) {
            ids.push(parsed);
        }
    }
    Ok(ids)
}

/// Starts one account's backend, unless nothing is linked: pairing happens in
/// the window, so an account without a session never connects from here.
fn open_account(
    dirs: &AppDirs,
    id: &AccountId,
    settings: &Settings,
    config: &Config,
) -> anyhow::Result<Arc<AccountHandle>> {
    let account_dirs = dirs.account(id);
    account_dirs
        .ensure()
        .with_context(|| format!("could not prepare the folders of account {id}"))?;
    if !account_dirs.session_db().exists() {
        log::warn!("account {id} is not linked; the MCP server will not connect it");
        return Ok(Arc::new(AccountHandle::unlinked(
            id.to_string(),
            config.timeout,
        )));
    }
    let mut backend = Backend::spawn(account_dirs, Waker::default());
    backend.send(Command::SetKeepChatsArchived(settings.keep_chats_archived));
    // The window waits for its first frame before opening storage; there is
    // no window here, so go ahead.
    if let Some(startup) = backend.take_startup() {
        let _ = startup.send(());
    }
    Ok(Arc::new(AccountHandle::new(
        id.to_string(),
        backend,
        LinkState::Starting,
        config.timeout,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster(order: &[&str], active: &str) -> AccountRoster {
        AccountRoster {
            next_id: 9,
            active: active.into(),
            order: order.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    #[test]
    fn the_active_account_is_served_by_default() {
        let ids = chosen_accounts(&roster(&["1", "2"], "2"), &Options::default()).unwrap();
        assert_eq!(ids, [AccountId::from("2")]);
        let ids = chosen_accounts(&roster(&[], "1"), &Options::default()).unwrap();
        assert_eq!(ids, [AccountId::from("1")]);
    }

    #[test]
    fn named_accounts_must_exist_and_are_served_once() {
        let roster = roster(&["1", "2"], "1");
        let ask = |ids: &[&str]| Options {
            accounts: ids.iter().map(|id| (*id).to_owned()).collect(),
        };
        let ids = chosen_accounts(&roster, &ask(&["2", "1", "2"])).unwrap();
        assert_eq!(ids, [AccountId::from("2"), AccountId::from("1")]);
        for bad in ["3", "../1", "", "01"] {
            assert!(chosen_accounts(&roster, &ask(&[bad])).is_err(), "{bad:?}");
        }
    }
}
