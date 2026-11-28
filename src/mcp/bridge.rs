//! The MCP server's side of the `Command` / `Event` channels.
//!
//! One [`AccountHandle`] talks to one account's backend. Standalone it owns the
//! [`Backend`], the same handle the window uses, and a pump drains its events;
//! inside the window it only holds a command sender, and the window feeds it
//! each event. Either way the events go into a small mirror of what
//! tools read (the chat list, the contacts, the link state) and republishes the
//! ones a tool may be waiting for. A tool takes a [`Session`], which holds the
//! account's turn so that answers cannot be mistaken for each other (events
//! carry no request id), sends `Command`s, and waits for the matching `Event`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::broadcast;

use super::phone::{self, Target};
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::{Backend, Command, Event, LinkStatus, PageKey, Refusal};
use crate::model::{Chat, ChatId, Contact, Delivery, Message};

/// How often the pump looks for events. The window is woken by the backend;
/// a headless server has no window to wake, so it looks instead.
/// Request id for number lookups the MCP bridge makes; the window counts its
/// own from zero, so this one never answers one of them.
const MCP_CONTACT_REQUEST: u64 = u64::MAX;

const PUMP_EVERY: Duration = Duration::from_millis(20);

/// How long the first chat list may take: the archive is unlocked through the
/// keyring and an upgraded archive waits for the phone's lock state.
const STARTUP_WAIT: Duration = Duration::from_secs(45);

/// How long a read mark waits to be applied.
const MARK_WAIT: Duration = Duration::from_secs(5);

/// A tool failure, worded for the agent that called it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    /// The message the failure is about, when one was already created.
    pub message_id: Option<String>,
}

impl ToolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            message_id: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_params", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    fn timeout(what: &str) -> Self {
        Self::new(
            "timeout",
            format!("ZapFast did not answer in time ({what}). Try again."),
        )
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The link state, without the pairing credentials `LinkStatus` carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Starting,
    /// No phone is linked. Linking happens in the ZapFast window only.
    NeedsLinking,
    Connecting,
    Connected,
    Disconnected,
    LoggedOut,
    Failed(String),
}

impl LinkState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::NeedsLinking => "needs_linking",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::LoggedOut => "logged_out",
            Self::Failed(_) => "failed",
        }
    }
}

impl From<&LinkStatus> for LinkState {
    fn from(status: &LinkStatus) -> Self {
        match status {
            LinkStatus::Starting => Self::Starting,
            // The QR code and pairing code stay behind.
            LinkStatus::Unlinked { .. } => Self::NeedsLinking,
            LinkStatus::Connecting => Self::Connecting,
            LinkStatus::Connected => Self::Connected,
            LinkStatus::Disconnected { .. } => Self::Disconnected,
            LinkStatus::LoggedOut => Self::LoggedOut,
            LinkStatus::Failed(reason) => Self::Failed(shorten(reason, 200)),
        }
    }
}

/// What tools read without asking the backend.
#[derive(Debug)]
pub struct Mirror {
    pub link: LinkState,
    pub me: Option<String>,
    pub me_name: Option<String>,
    pub chats: Vec<Chat>,
    pub contacts: HashMap<String, Contact>,
    pub chats_loaded: bool,
}

impl Mirror {
    fn new(link: LinkState) -> Self {
        Self {
            link,
            me: None,
            me_name: None,
            chats: Vec::new(),
            contacts: HashMap::new(),
            chats_loaded: false,
        }
    }
}

/// The events a tool can be waiting for.
#[derive(Clone, Debug)]
pub enum Observed {
    /// A page of one chat's archive, from `Command::LoadChat`.
    Page {
        chat: ChatId,
        messages: Vec<Message>,
        older: bool,
        complete: bool,
    },
    /// A message that just arrived or was just filed, ours or theirs.
    Live {
        chat: ChatId,
        message: Message,
    },
    Updated(Message),
    ChatHits {
        chat: ChatId,
        query: String,
        from: Option<i64>,
        until: Option<i64>,
        messages: Vec<Message>,
        truncated: bool,
    },
    SearchHits {
        query: String,
        messages: Vec<Message>,
    },
    ContactReady {
        id: String,
    },
    ChatChanged(ChatId),
    Refused {
        chat: ChatId,
        reason: Refusal,
    },
    Failure(String),
}

/// How commands reach the backend, and whether events must be fetched.
enum Port {
    /// This server owns the backend and polls its events.
    Owned(Mutex<Backend>),
    /// The window owns it: commands go through a clone of its sender, and the
    /// window passes events to [`AccountHandle::feed`].
    Shared(UnboundedSender<Command>),
}

/// One account: its backend, its mirror, and the turn that serializes tools.
pub struct AccountHandle {
    pub id: String,
    port: Port,
    mirror: Mutex<Mirror>,
    observed: broadcast::Sender<Observed>,
    turn: tokio::sync::Mutex<()>,
    stopped: AtomicBool,
    timeout: Duration,
}

impl AccountHandle {
    pub fn new(
        id: impl Into<String>,
        backend: Backend,
        link: LinkState,
        timeout: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            port: Port::Owned(Mutex::new(backend)),
            mirror: Mutex::new(Mirror::new(link)),
            observed: broadcast::channel(1024).0,
            turn: tokio::sync::Mutex::new(()),
            stopped: AtomicBool::new(false),
            timeout,
        }
    }

    /// An account with nothing to run: not linked, so the server never
    /// connects for it.
    pub fn unlinked(id: impl Into<String>, timeout: Duration) -> Self {
        let (backend, _events) = Backend::detached();
        Self::new(id, backend, LinkState::NeedsLinking, timeout)
    }

    /// An account the window owns. `commands` is a clone of its backend's
    /// sender.
    pub fn shared(
        id: impl Into<String>,
        commands: UnboundedSender<Command>,
        timeout: Duration,
    ) -> Self {
        let mut handle = Self::unlinked(id, timeout);
        handle.port = Port::Shared(commands);
        handle.mirror().link = LinkState::Starting;
        handle
    }

    fn mirror(&self) -> MutexGuard<'_, Mirror> {
        self.mirror
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn send(&self, command: Command) {
        match &self.port {
            Port::Owned(backend) => backend
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .send(command),
            Port::Shared(commands) => {
                let _ = commands.send(command);
            }
        }
    }

    pub fn link(&self) -> LinkState {
        self.mirror().link.clone()
    }

    pub fn me(&self) -> (Option<String>, Option<String>) {
        let mirror = self.mirror();
        (mirror.me.clone(), mirror.me_name.clone())
    }

    /// The chats a tool may show: everything except chats the phone locked.
    pub fn visible_chats(&self) -> Vec<Chat> {
        let mut chats: Vec<Chat> = self
            .mirror()
            .chats
            .iter()
            .filter(|chat| !chat.locked)
            .cloned()
            .collect();
        chats.sort_by_key(|chat| std::cmp::Reverse((chat.pinned, chat.last_activity)));
        chats
    }

    pub fn visible_chat(&self, id: &str) -> Option<Chat> {
        self.mirror()
            .chats
            .iter()
            .find(|chat| chat.id == id && !chat.locked)
            .cloned()
    }

    pub fn contact(&self, id: &str) -> Option<Contact> {
        self.mirror().contacts.get(id).cloned()
    }

    /// The best name for a sender, from the address book and the chat list.
    pub fn name_of(&self, id: &str) -> Option<String> {
        let mirror = self.mirror();
        mirror
            .contacts
            .get(id)
            .and_then(|contact| contact.display_name().map(str::to_owned))
            .or_else(|| {
                mirror
                    .chats
                    .iter()
                    .find(|chat| chat.id == id && !chat.locked)
                    .map(|chat| chat.name.clone())
            })
    }

    /// Takes what the backend has said since the last call.
    pub fn pump(&self) -> usize {
        let Port::Owned(backend) = &self.port else {
            return 0;
        };
        let events = backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .poll();
        let count = events.len();
        for event in &events {
            self.feed(event);
        }
        count
    }

    /// Keeps pumping until [`AccountHandle::shutdown`].
    pub fn spawn_pump(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let handle = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PUMP_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while !handle.stopped.load(Ordering::Relaxed) {
                tick.tick().await;
                handle.pump();
            }
        })
    }

    /// Stops the pump and the backend, waiting for its thread. Blocking.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Port::Owned(backend) = &self.port {
            backend
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .shutdown();
        }
    }

    /// Takes one event the backend emitted.
    pub fn feed(&self, event: &Event) {
        let observed = match event {
            Event::Link(status) => {
                self.mirror().link = LinkState::from(status);
                return;
            }
            Event::Me { id, name, .. } => {
                let mut mirror = self.mirror();
                mirror.me = Some(id.clone());
                mirror.me_name = name.clone();
                return;
            }
            Event::Chats(chats) => {
                let mut mirror = self.mirror();
                mirror.chats = chats.clone();
                mirror.chats_loaded = true;
                return;
            }
            Event::ChatUpdated(chat) => {
                let id = chat.id.clone();
                let mut mirror = self.mirror();
                match mirror.chats.iter_mut().find(|known| known.id == id) {
                    Some(known) => *known = (**chat).clone(),
                    None => mirror.chats.push((**chat).clone()),
                }
                drop(mirror);
                Observed::ChatChanged(id)
            }
            Event::ChatRemoved { chat } => {
                self.mirror().chats.retain(|known| known.id != *chat);
                return;
            }
            Event::Contacts(contacts) => {
                let mut mirror = self.mirror();
                for contact in contacts {
                    mirror.contacts.insert(contact.id.clone(), contact.clone());
                }
                return;
            }
            Event::Messages {
                chat,
                messages,
                older,
                complete,
            } => {
                // A page is full or says it is complete. A lone message that
                // claims neither is one that just arrived or was just filed.
                if !older && !complete && messages.len() == 1 {
                    Observed::Live {
                        chat: chat.clone(),
                        message: messages[0].clone(),
                    }
                } else {
                    Observed::Page {
                        chat: chat.clone(),
                        messages: messages.clone(),
                        older: *older,
                        complete: *complete,
                    }
                }
            }
            Event::MessageUpdated(message) => Observed::Updated((**message).clone()),
            Event::ChatHits {
                chat,
                query,
                from,
                until,
                messages,
                truncated,
            } => Observed::ChatHits {
                chat: chat.clone(),
                query: query.clone(),
                from: *from,
                until: *until,
                messages: messages.clone(),
                truncated: *truncated,
            },
            Event::SearchHits { query, messages } => Observed::SearchHits {
                query: query.clone(),
                messages: messages.clone(),
            },
            Event::ContactReady { id, .. } => Observed::ContactReady { id: id.clone() },
            Event::SendRefused { chat, reason, .. } => Observed::Refused {
                chat: chat.clone(),
                reason: *reason,
            },
            Event::Error(error) => Observed::Failure(shorten(error, 300)),
            _ => return,
        };
        let _ = self.observed.send(observed);
    }

    /// Waits until the chat list is known, or says why it cannot be.
    pub async fn ready(&self) -> Result<(), ToolError> {
        let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
        loop {
            let (loaded, link) = {
                let mirror = self.mirror();
                (mirror.chats_loaded, mirror.link.clone())
            };
            match link {
                LinkState::NeedsLinking | LinkState::LoggedOut => {
                    return Err(ToolError::new(
                        "not_linked",
                        "This account is not linked to a phone. Open the ZapFast window, link \
                         the phone (Settings, Linked devices), close the window, and start \
                         the MCP server again.",
                    ));
                }
                LinkState::Failed(reason) => {
                    return Err(ToolError::new(
                        "backend_error",
                        format!("ZapFast could not open this account: {reason}"),
                    ));
                }
                _ => {}
            }
            if loaded {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ToolError::new(
                    "not_ready",
                    "ZapFast is still opening this account's archive. Try again in a moment.",
                ));
            }
            tokio::time::sleep(PUMP_EVERY).await;
        }
    }

    /// Takes this account's turn, once the chat list is known.
    pub async fn session(&self) -> Result<Session<'_>, ToolError> {
        let turn = self.turn.lock().await;
        let events = self.observed.subscribe();
        self.ready().await?;
        Ok(Session {
            handle: self,
            events,
            _turn: turn,
        })
    }

    // --- tool operations -------------------------------------------------

    /// The newest `limit` messages of a chat, or the ones before `before`,
    /// oldest first, and the cursor for the page before them.
    pub async fn messages(
        &self,
        chat: &str,
        before: Option<PageKey>,
        limit: usize,
    ) -> Result<(Vec<Message>, Option<PageKey>), ToolError> {
        let mut session = self.session().await?;
        let mut collected: Vec<Message> = Vec::new();
        let mut cursor = before;
        let complete = loop {
            let older = cursor.is_some();
            session.send(Command::LoadChat {
                chat: chat.to_owned(),
                before: cursor.clone(),
            });
            let (page, complete) = session
                .wait(self.timeout, |observed| match observed {
                    Observed::Page {
                        chat: page_chat,
                        messages,
                        older: page_older,
                        complete,
                    } if page_chat == chat && *page_older == older => {
                        Some((messages.clone(), *complete))
                    }
                    _ => None,
                })
                .await
                .ok_or_else(|| ToolError::timeout("reading messages"))?;
            let empty = page.is_empty();
            collected.splice(0..0, page);
            if complete || empty || collected.len() >= limit {
                break complete || empty;
            }
            cursor = collected
                .first()
                .map(|first| (first.timestamp, first.id.clone()));
        };
        let skipped = collected.len().saturating_sub(limit);
        let messages = collected.split_off(skipped);
        let next = (!complete || skipped > 0)
            .then(|| {
                messages
                    .first()
                    .map(|first| (first.timestamp, first.id.clone()))
            })
            .flatten();
        Ok((messages, next))
    }

    /// Searches every chat (at most 50 hits) or one chat (at most 80).
    pub async fn search(
        &self,
        query: &str,
        chat: Option<&str>,
        from: Option<i64>,
        until: Option<i64>,
    ) -> Result<(Vec<Message>, bool), ToolError> {
        let mut session = self.session().await?;
        match chat {
            Some(chat) => {
                session.send(Command::SearchChatMessages {
                    chat: chat.to_owned(),
                    query: query.to_owned(),
                    from,
                    until,
                });
                session
                    .wait(self.timeout, |observed| match observed {
                        Observed::ChatHits {
                            chat: hit_chat,
                            query: hit_query,
                            from: hit_from,
                            until: hit_until,
                            messages,
                            truncated,
                        } if hit_chat == chat
                            && hit_query == query
                            && *hit_from == from
                            && *hit_until == until =>
                        {
                            Some((messages.clone(), *truncated))
                        }
                        _ => None,
                    })
                    .await
                    .ok_or_else(|| ToolError::timeout("searching"))
            }
            None => {
                session.send(Command::SearchMessages {
                    query: query.to_owned(),
                });
                let (messages, _) = session
                    .wait(self.timeout, |observed| match observed {
                        Observed::SearchHits {
                            query: hit_query,
                            messages,
                        } if hit_query == query => Some((messages.clone(), false)),
                        _ => None,
                    })
                    .await
                    .ok_or_else(|| ToolError::timeout("searching"))?;
                // The backend's global search has no range: narrow it here.
                let messages = messages
                    .into_iter()
                    .filter(|message| {
                        from.is_none_or(|from| message.timestamp >= from)
                            && until.is_none_or(|until| message.timestamp < until)
                    })
                    .collect();
                Ok((messages, false))
            }
        }
    }

    /// Sends text through `Command::SendText` and waits for the server's
    /// first acknowledgement.
    pub async fn send_text(
        &self,
        target: &Target,
        text: String,
        reply_to: Option<String>,
    ) -> Result<Delivered, ToolError> {
        let mut session = self.session().await?;
        let chat = self.resolve_for_send(&mut session, target).await?;
        session.send(Command::SendText {
            chat: chat.clone(),
            text,
            quoting: reply_to.map(|id| crate::model::ReplyTarget { id, chat: None }),
            mentions: Vec::new(),
            preview: None,
        });
        self.confirm(&mut session, chat).await
    }

    /// Sends one file through `Command::SendFiles`. The caller has vetted
    /// the path.
    pub async fn send_file(
        &self,
        target: &Target,
        path: std::path::PathBuf,
        caption: Option<String>,
    ) -> Result<Delivered, ToolError> {
        let mut session = self.session().await?;
        let chat = self.resolve_for_send(&mut session, target).await?;
        session.send(Command::SendFiles {
            chat: chat.clone(),
            paths: vec![path],
            documents: false,
            view_once: false,
            caption,
            mentions: Vec::new(),
            quoting: None,
        });
        self.confirm(&mut session, chat).await
    }

    /// Marks a chat read through `Command::MarkRead`. Returns the chat as it
    /// is afterwards.
    pub async fn mark_read(&self, chat: &str, receipts: bool) -> Result<Chat, ToolError> {
        let mut session = self.session().await?;
        session.send(Command::MarkRead {
            chat: chat.to_owned(),
            receipts,
        });
        session
            .wait(MARK_WAIT.min(self.timeout), |observed| match observed {
                Observed::ChatChanged(changed) if changed == chat => Some(()),
                _ => None,
            })
            .await
            .ok_or_else(|| ToolError::timeout("marking the chat read"))?;
        self.visible_chat(chat)
            .ok_or_else(|| ToolError::not_found("The chat is no longer available."))
    }

    /// The chat a send goes to. A number nobody has written to yet is checked
    /// with WhatsApp (`Command::NewContact`) before anything is filed.
    async fn resolve_for_send(
        &self,
        session: &mut Session<'_>,
        target: &Target,
    ) -> Result<ChatId, ToolError> {
        let digits = match target {
            Target::Chat(id) => {
                if self.visible_chat(id).is_some() {
                    return Ok(id.clone());
                }
                match crate::model::phone_of(id) {
                    Some(digits) => digits.to_owned(),
                    None => {
                        return Err(ToolError::not_found(
                            "No chat has this id. Use whatsapp_list_chats to find it.",
                        ));
                    }
                }
            }
            Target::Phone(digits) => digits.clone(),
        };
        if let Some(chat) = phone::find_by_phone(&self.visible_chats(), &digits) {
            return Ok(chat.id.clone());
        }
        session.send(Command::NewContact {
            request: MCP_CONTACT_REQUEST,
            phone: digits,
            full_name: None,
            first_name: None,
            to_phone: false,
        });
        session
            .wait(self.timeout, |observed| match observed {
                Observed::ContactReady { id } => Some(Ok(id.clone())),
                Observed::Failure(error) => Some(Err(ToolError::not_found(error.clone()))),
                _ => None,
            })
            .await
            .ok_or_else(|| ToolError::timeout("checking the number"))?
    }

    /// Waits for the message the command just filed and for its first
    /// acknowledgement.
    async fn confirm(
        &self,
        session: &mut Session<'_>,
        chat: ChatId,
    ) -> Result<Delivered, ToolError> {
        let filed = session
            .wait(self.timeout, |observed| match observed {
                Observed::Live {
                    chat: filed_chat,
                    message,
                } if *filed_chat == chat && message.from_me => Some(Ok(message.clone())),
                Observed::Refused {
                    chat: refused,
                    reason,
                } if *refused == chat => Some(Err(match reason {
                    Refusal::Offline => ToolError::new(
                        "offline",
                        "ZapFast is not connected to WhatsApp right now. Nothing was sent.",
                    ),
                    Refusal::QuoteUnavailable => ToolError::not_found(
                        "The message in reply_to is not stored here or cannot be quoted. \
                         Nothing was sent.",
                    ),
                    Refusal::Blocked => {
                        ToolError::new("blocked", "This contact is blocked. Nothing was sent.")
                    }
                    Refusal::UnsupportedViewOnce => ToolError::new(
                        "unsupported",
                        "View-once sends are not supported here. Nothing was sent.",
                    ),
                })),
                Observed::Failure(error) => {
                    Some(Err(ToolError::new("backend_error", error.clone())))
                }
                _ => None,
            })
            .await
            .ok_or_else(|| ToolError::timeout("sending"))??;
        let id = filed.id.clone();
        let mut status = filed.status;
        if status == Delivery::Pending {
            status = session
                .wait(self.timeout, |observed| match observed {
                    Observed::Updated(message)
                        if message.chat == chat
                            && message.id == id
                            && message.status != Delivery::Pending =>
                    {
                        Some(message.status)
                    }
                    _ => None,
                })
                .await
                .unwrap_or(Delivery::Pending);
        }
        if status == Delivery::Failed {
            return Err(ToolError {
                code: "send_failed",
                message: "WhatsApp did not accept the message. It stays in the chat marked \
                          as failed."
                    .into(),
                message_id: Some(id),
            });
        }
        Ok(Delivered { chat, id, status })
    }
}

/// A message the server took, or is about to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub chat: ChatId,
    pub id: String,
    /// `Pending` when the acknowledgement had not arrived in time.
    pub status: Delivery,
}

/// An account's turn: commands go out and the answers are read back without
/// another tool's answers mixed in.
pub struct Session<'a> {
    handle: &'a AccountHandle,
    events: broadcast::Receiver<Observed>,
    _turn: tokio::sync::MutexGuard<'a, ()>,
}

impl Session<'_> {
    pub fn send(&self, command: Command) {
        self.handle.send(command);
    }

    /// The first event `accept` takes within `within`, or `None`.
    pub async fn wait<T>(
        &mut self,
        within: Duration,
        mut accept: impl FnMut(&Observed) -> Option<T>,
    ) -> Option<T> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Ok(observed)) => {
                    if let Some(found) = accept(&observed) {
                        return Some(found);
                    }
                }
                // Missed events may have held the answer; keep waiting for
                // a repeat rather than guess.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }
}

/// `text` cut to `limit` characters, with an ellipsis when it was longer.
pub fn shorten(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The cursor a result hands back for the page before it.
pub fn encode_cursor(key: &PageKey) -> String {
    format!("{}:{}", key.0, key.1)
}

pub fn decode_cursor(cursor: &str) -> Option<PageKey> {
    let (timestamp, id) = cursor.split_once(':')?;
    (!id.is_empty() && id.len() <= 128)
        .then(|| {
            timestamp
                .parse()
                .ok()
                .map(|timestamp| (timestamp, id.to_owned()))
        })
        .flatten()
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! A scripted stand-in for the worker: it reads the commands the server
    //! sends and answers with the events the real worker emits.

    use super::*;
    use crate::model::Content;
    use tokio::sync::mpsc::UnboundedReceiver;

    pub fn chat(id: &str, name: &str, last_activity: i64) -> Chat {
        let mut chat = Chat::new(id.into(), name.into());
        chat.last_activity = last_activity;
        chat
    }

    pub fn message(chat: &str, id: &str, timestamp: i64, text: &str, from_me: bool) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: if from_me {
                "5511900000000@s.whatsapp.net".into()
            } else {
                chat.into()
            },
            sender_name: None,
            from_me,
            timestamp,
            history_order: None,
            content: Content::text(text),
            status: if from_me {
                Delivery::Pending
            } else {
                Delivery::None
            },
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        }
    }

    pub struct Rig {
        pub handle: Arc<AccountHandle>,
        pub commands: UnboundedReceiver<Command>,
        pub events: std::sync::mpsc::Sender<Event>,
        pub _pump: tokio::task::JoinHandle<()>,
    }

    /// A connected account whose chat list is already known.
    pub fn rig(chats: Vec<Chat>) -> Rig {
        let (backend, commands, events) = Backend::recording_with_events();
        let handle = Arc::new(AccountHandle::new(
            "1",
            backend,
            LinkState::Connected,
            Duration::from_millis(400),
        ));
        events.send(Event::Chats(chats)).unwrap();
        let pump = handle.spawn_pump();
        Rig {
            handle,
            commands,
            events,
            _pump: pump,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::model::Content;

    const ANA: &str = "5562999999999@s.whatsapp.net";

    #[test]
    fn cursors_round_trip_and_reject_junk() {
        let key = (1_700_000_000, "3EB0ABC:1".to_owned());
        assert_eq!(decode_cursor(&encode_cursor(&key)), Some(key));
        for junk in ["", "abc", "12:", ":id", "x:id"] {
            assert_eq!(decode_cursor(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        assert_eq!(shorten("héllo wörld", 5), "héllo…");
        assert_eq!(shorten("short", 5), "short");
    }

    #[test]
    fn link_state_drops_pairing_credentials() {
        let status = LinkStatus::Unlinked {
            qr: Some("2@secretqr".into()),
            pair_code: Some("ABCD1234".into()),
            pairing_phone: Some("5562999999999".into()),
        };
        let state = LinkState::from(&status);
        assert_eq!(state, LinkState::NeedsLinking);
        assert!(!format!("{state:?}").contains("secretqr"));
    }

    #[tokio::test]
    async fn locked_chats_are_never_visible() {
        let mut locked = chat("1@g.us", "Secret", 50);
        locked.locked = true;
        let rig = rig(vec![
            chat(ANA, "Ana", 10),
            locked,
            chat("2@g.us", "Team", 20),
        ]);
        rig.handle.pump();
        let names: Vec<_> = rig
            .handle
            .visible_chats()
            .into_iter()
            .map(|chat| chat.name)
            .collect();
        assert_eq!(names, ["Team", "Ana"]);
        assert!(rig.handle.visible_chat("1@g.us").is_none());
    }

    #[tokio::test]
    async fn an_unlinked_account_asks_for_linking_without_waiting() {
        let handle = AccountHandle::unlinked("1", Duration::from_millis(100));
        let error = handle.messages(ANA, None, 10).await.unwrap_err();
        assert_eq!(error.code, "not_linked");
    }

    #[tokio::test]
    async fn messages_page_back_until_the_limit_is_met() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        let worker = tokio::spawn(async move {
            let mut asked = Vec::new();
            while let Some(command) = rig.commands.recv().await {
                let Command::LoadChat { chat, before } = command else {
                    continue;
                };
                asked.push(before.clone());
                let (range, complete) = match before {
                    None => (61..=120, false),
                    Some(_) => (1..=60, true),
                };
                let messages = range
                    .map(|n| message(&chat, &format!("m{n}"), n, "hi", false))
                    .collect();
                events
                    .send(Event::Messages {
                        chat,
                        messages,
                        older: asked.last().is_some_and(Option::is_some),
                        complete,
                    })
                    .unwrap();
                if complete {
                    return asked;
                }
            }
            asked
        });
        let (messages, next) = handle.messages(ANA, None, 100).await.unwrap();
        assert_eq!(messages.len(), 100);
        assert_eq!(messages.first().unwrap().id, "m21");
        assert_eq!(messages.last().unwrap().id, "m120");
        assert!(
            messages
                .windows(2)
                .all(|pair| pair[0].timestamp < pair[1].timestamp)
        );
        // 20 older messages were fetched and left out, so there is a cursor.
        assert_eq!(next, Some((21, "m21".to_owned())));
        let asked = worker.await.unwrap();
        assert_eq!(asked, [None, Some((61, "m61".to_owned()))]);
    }

    #[tokio::test]
    async fn a_complete_page_has_no_older_cursor() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        tokio::spawn(async move {
            while let Some(Command::LoadChat { chat, .. }) = rig.commands.recv().await {
                let messages = vec![message(&chat, "only", 5, "hello", false)];
                events
                    .send(Event::Messages {
                        chat,
                        messages,
                        older: false,
                        complete: true,
                    })
                    .unwrap();
            }
        });
        let (messages, next) = handle.messages(ANA, None, 30).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(next, None);
    }

    #[tokio::test]
    async fn a_message_arriving_mid_read_is_not_taken_for_the_page() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        tokio::spawn(async move {
            while let Some(Command::LoadChat { chat, .. }) = rig.commands.recv().await {
                // A live message lands first: one row, not complete.
                events
                    .send(Event::Messages {
                        chat: chat.clone(),
                        messages: vec![message(&chat, "live", 99, "new", false)],
                        older: false,
                        complete: false,
                    })
                    .unwrap();
                events
                    .send(Event::Messages {
                        chat: chat.clone(),
                        messages: vec![
                            message(&chat, "a", 1, "one", false),
                            message(&chat, "b", 2, "two", false),
                        ],
                        older: false,
                        complete: true,
                    })
                    .unwrap();
            }
        });
        let (messages, _) = handle.messages(ANA, None, 30).await.unwrap();
        let ids: Vec<_> = messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[tokio::test]
    async fn global_search_is_narrowed_to_the_range() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        tokio::spawn(async move {
            while let Some(Command::SearchMessages { query }) = rig.commands.recv().await {
                let messages = (1..=4)
                    .map(|n| message(ANA, &format!("m{n}"), n * 100, &query, false))
                    .collect();
                events.send(Event::SearchHits { query, messages }).unwrap();
            }
        });
        let (hits, truncated) = handle
            .search("invoice", None, Some(200), Some(400))
            .await
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m2", "m3"]);
        assert!(!truncated);
    }

    #[tokio::test]
    async fn chat_search_sends_the_range_and_matches_the_echo() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        let worker = tokio::spawn(async move {
            let command = rig.commands.recv().await.unwrap();
            let Command::SearchChatMessages {
                chat,
                query,
                from,
                until,
            } = command
            else {
                panic!("unexpected command");
            };
            // An answer to a different question must be ignored.
            events
                .send(Event::ChatHits {
                    chat: chat.clone(),
                    query: "other".into(),
                    from,
                    until,
                    messages: vec![message(&chat, "wrong", 1, "x", false)],
                    truncated: false,
                })
                .unwrap();
            events
                .send(Event::ChatHits {
                    chat: chat.clone(),
                    query: query.clone(),
                    from,
                    until,
                    messages: vec![message(&chat, "right", 2, "x", false)],
                    truncated: true,
                })
                .unwrap();
            (from, until)
        });
        let (hits, truncated) = handle
            .search("x", Some(ANA), Some(10), Some(20))
            .await
            .unwrap();
        assert_eq!(hits[0].id, "right");
        assert!(truncated);
        assert_eq!(worker.await.unwrap(), (Some(10), Some(20)));
    }

    /// Plays the worker for one `SendText`: files a pending row, then acks.
    fn answer_send(mut rig: Rig, ack: Delivery) -> tokio::task::JoinHandle<Vec<Command>> {
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(command) = rig.commands.recv().await {
                let chat = match &command {
                    Command::SendText { chat, .. } | Command::SendFiles { chat, .. } => {
                        chat.clone()
                    }
                    Command::NewContact { phone, .. } => {
                        rig.events
                            .send(Event::ContactReady {
                                request: MCP_CONTACT_REQUEST,
                                id: format!("{phone}@s.whatsapp.net"),
                                name: None,
                            })
                            .unwrap();
                        seen.push(command);
                        continue;
                    }
                    _ => {
                        seen.push(command);
                        continue;
                    }
                };
                seen.push(command);
                let row = message(&chat, "SENT1", 42, "text", true);
                rig.events
                    .send(Event::Messages {
                        chat: chat.clone(),
                        messages: vec![row.clone()],
                        older: false,
                        complete: false,
                    })
                    .unwrap();
                let mut acked = row;
                acked.status = ack;
                rig.events
                    .send(Event::MessageUpdated(Box::new(acked)))
                    .unwrap();
                return seen;
            }
            seen
        })
    }

    #[tokio::test]
    async fn sending_text_to_a_known_chat_issues_send_text_and_waits_for_the_ack() {
        let rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let worker = answer_send(rig, Delivery::Sent);
        let delivered = handle
            .send_text(
                &Target::Chat(ANA.into()),
                "Olá".into(),
                Some("QUOTED".into()),
            )
            .await
            .unwrap();
        assert_eq!(delivered.id, "SENT1");
        assert_eq!(delivered.status, Delivery::Sent);
        let commands = worker.await.unwrap();
        let [
            Command::SendText {
                chat,
                text,
                quoting,
                mentions,
                ..
            },
        ] = &commands[..]
        else {
            panic!("unexpected commands: {commands:?}");
        };
        assert_eq!(chat, ANA);
        assert_eq!(text, "Olá");
        assert_eq!(quoting.as_ref().map(|q| q.id.as_str()), Some("QUOTED"));
        assert!(mentions.is_empty());
    }

    #[tokio::test]
    async fn a_known_number_resolves_to_its_chat_without_a_lookup() {
        let rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let worker = answer_send(rig, Delivery::Sent);
        handle
            .send_text(&Target::Phone("5562999999999".into()), "oi".into(), None)
            .await
            .unwrap();
        let commands = worker.await.unwrap();
        assert!(matches!(&commands[..], [Command::SendText { chat, .. }] if chat == ANA));
    }

    #[tokio::test]
    async fn a_new_number_is_checked_with_whatsapp_before_sending() {
        let rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let worker = answer_send(rig, Delivery::Sent);
        handle
            .send_text(&Target::Phone("5511988887777".into()), "oi".into(), None)
            .await
            .unwrap();
        let commands = worker.await.unwrap();
        assert!(matches!(
            &commands[..],
            [
                Command::NewContact { phone, full_name: None, to_phone: false, .. },
                Command::SendText { chat, .. },
            ] if phone == "5511988887777" && chat == "5511988887777@s.whatsapp.net"
        ));
    }

    #[tokio::test]
    async fn a_number_not_on_whatsapp_sends_nothing() {
        let mut rig = rig(vec![]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        let worker = tokio::spawn(async move {
            let mut commands = Vec::new();
            while let Some(command) = rig.commands.recv().await {
                if matches!(command, Command::NewContact { .. }) {
                    events
                        .send(Event::Error("+55 11 98888-7777 is not on WhatsApp".into()))
                        .unwrap();
                }
                commands.push(command);
                if commands.len() == 1 {
                    break;
                }
            }
            commands
        });
        let error = handle
            .send_text(&Target::Phone("5511988887777".into()), "oi".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, "not_found");
        assert!(error.message.contains("not on WhatsApp"));
        let commands = worker.await.unwrap();
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, Command::SendText { .. }))
        );
    }

    #[tokio::test]
    async fn sending_to_an_unknown_group_is_refused_before_any_command() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let error = rig
            .handle
            .send_text(&Target::Chat("999@g.us".into()), "hi".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, "not_found");
        assert!(rig.commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_refused_send_reports_why() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        tokio::spawn(async move {
            while let Some(Command::SendText {
                chat,
                text,
                quoting,
                ..
            }) = rig.commands.recv().await
            {
                events
                    .send(Event::SendRefused {
                        chat,
                        quoting,
                        unsent: crate::backend::Unsent::Text(text),
                        reason: Refusal::Offline,
                    })
                    .unwrap();
            }
        });
        let error = handle
            .send_text(&Target::Chat(ANA.into()), "hi".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, "offline");
    }

    #[tokio::test]
    async fn a_failed_acknowledgement_is_an_error_that_names_the_message() {
        let rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let _worker = answer_send(rig, Delivery::Failed);
        let error = handle
            .send_text(&Target::Chat(ANA.into()), "hi".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, "send_failed");
        assert_eq!(error.message_id.as_deref(), Some("SENT1"));
    }

    #[tokio::test]
    async fn a_missing_acknowledgement_reports_the_message_as_pending() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        tokio::spawn(async move {
            while let Some(Command::SendText { chat, .. }) = rig.commands.recv().await {
                events
                    .send(Event::Messages {
                        chat: chat.clone(),
                        messages: vec![message(&chat, "P1", 1, "x", true)],
                        older: false,
                        complete: false,
                    })
                    .unwrap();
            }
        });
        let delivered = handle
            .send_text(&Target::Chat(ANA.into()), "x".into(), None)
            .await
            .unwrap();
        assert_eq!(delivered.status, Delivery::Pending);
    }

    #[tokio::test]
    async fn files_go_through_send_files_with_the_caption() {
        let rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let worker = answer_send(rig, Delivery::Sent);
        handle
            .send_file(
                &Target::Chat(ANA.into()),
                "/tmp/report.pdf".into(),
                Some("Here".into()),
            )
            .await
            .unwrap();
        let commands = worker.await.unwrap();
        let [
            Command::SendFiles {
                chat,
                paths,
                caption,
                quoting,
                mentions,
                ..
            },
        ] = &commands[..]
        else {
            panic!("unexpected commands: {commands:?}");
        };
        assert_eq!(chat, ANA);
        assert_eq!(paths, &[std::path::PathBuf::from("/tmp/report.pdf")]);
        assert_eq!(caption.as_deref(), Some("Here"));
        assert!(quoting.is_none() && mentions.is_empty());
    }

    #[tokio::test]
    async fn marking_read_issues_mark_read_and_returns_the_updated_chat() {
        let mut rig = rig(vec![chat(ANA, "Ana", 10)]);
        let handle = rig.handle.clone();
        let events = rig.events.clone();
        let worker = tokio::spawn(async move {
            let Some(Command::MarkRead { chat, receipts }) = rig.commands.recv().await else {
                panic!("expected MarkRead");
            };
            let mut read = crate::mcp::bridge::fixtures::chat(&chat, "Ana", 10);
            read.unread = 0;
            events.send(Event::ChatUpdated(Box::new(read))).unwrap();
            (chat, receipts)
        });
        let chat = handle.mark_read(ANA, false).await.unwrap();
        assert_eq!(chat.unread, 0);
        assert_eq!(worker.await.unwrap(), (ANA.to_owned(), false));
    }

    #[tokio::test]
    async fn the_mirror_follows_chat_and_contact_events() {
        let rig = rig(vec![chat(ANA, "Ana", 10)]);
        rig.handle.pump();
        rig.events
            .send(Event::Contacts(vec![Contact {
                id: ANA.into(),
                full_name: Some("Ana Souza".into()),
                first_name: None,
                push_name: Some("ana".into()),
            }]))
            .unwrap();
        let mut renamed = chat(ANA, "Ana S.", 99);
        renamed.unread = 3;
        rig.events
            .send(Event::ChatUpdated(Box::new(renamed)))
            .unwrap();
        rig.events
            .send(Event::Link(LinkStatus::Disconnected { reason: "x".into() }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(rig.handle.visible_chat(ANA).unwrap().unread, 3);
        assert_eq!(rig.handle.name_of(ANA).as_deref(), Some("Ana Souza"));
        assert_eq!(rig.handle.link(), LinkState::Disconnected);
        rig.events
            .send(Event::ChatRemoved { chat: ANA.into() })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(rig.handle.visible_chat(ANA).is_none());
        let _ = Content::Revoked;
    }
}
