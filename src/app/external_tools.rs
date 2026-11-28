//! The window's side of external tools: what the Forward dialog's messages
//! become (the Ctrl+C transcript and the downloaded image, video and audio
//! files), the downloads a send waits for, and the answer, which only ever
//! becomes a draft. Nothing here sends anything on WhatsApp.

use super::App;
use crate::backend::Command;
use crate::external_tools::{Tool, ToolFile, ToolKind};
use crate::model::{AccountId, Action, ChatId, Content, Dialog, MediaState};

/// A send waiting for its media to download.
pub(super) struct ToolJob {
    account: AccountId,
    tool: Tool,
    chat: ChatId,
    messages: Vec<String>,
    prompt: Option<String>,
}

enum Payload {
    Ready { text: String, files: Vec<ToolFile> },
    Waiting(Vec<String>),
    Failed,
    Missing,
}

impl App {
    /// The transcript and media of these messages, or what keeps them back.
    fn tool_payload(&self, chat: &str, messages: &[String]) -> Payload {
        let Some(text) = crate::ui::conversation::transcript_of(self, chat, messages) else {
            return Payload::Missing;
        };
        let Some(conversation) = self.conversations.get(chat) else {
            return Payload::Missing;
        };
        let mut files = Vec::new();
        let mut waiting = Vec::new();
        for message in conversation
            .messages
            .iter()
            .filter(|message| messages.contains(&message.id))
        {
            let Some(media) = sent_media(&message.content) else {
                continue;
            };
            match &media.path {
                Some(path) if path.is_file() => {
                    let extension = path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .unwrap_or("bin");
                    files.push(ToolFile {
                        path: path.clone(),
                        mime: media.mime.clone(),
                        name: format!("{}.{extension}", files.len() + 1),
                    });
                }
                _ if matches!(media.state, MediaState::Failed(_)) => return Payload::Failed,
                _ => waiting.push(message.id.clone()),
            }
        }
        if waiting.is_empty() {
            Payload::Ready { text, files }
        } else {
            Payload::Waiting(waiting)
        }
    }

    /// Shows the Forward dialog's step for the tool at this index of
    /// Settings. The prompt typed so far stays, so Back and another tool
    /// keep it.
    pub(super) fn pick_tool(&mut self, index: usize) {
        if matches!(self.dialog, Some(Dialog::Forward { .. }))
            && self
                .settings
                .external_tools
                .get(index)
                .is_some_and(Tool::usable)
        {
            self.tool_pick = Some(index);
            self.forward_reviewing = false;
        }
    }

    /// Sends the Forward dialog's messages to the tool at this index of
    /// Settings, downloading their media first.
    pub(super) fn send_to_tool(&mut self, index: usize) {
        let Some(Dialog::Forward { chat, messages }) = self.dialog.clone() else {
            return;
        };
        let Some(tool) = self
            .settings
            .external_tools
            .get(index)
            .filter(|tool| tool.usable())
            .cloned()
        else {
            return;
        };
        let name = tool.name.trim().to_owned();
        let prompt = self.tool_prompt_to_send();
        if tool.kind == ToolKind::Webhook
            && self.tool_media_size(&chat, &messages) > crate::model::ATTACHMENT_DOWNLOAD_LIMIT
        {
            self.toast_error(format!("The selected media is too large for {name}"));
            return;
        }
        let payload = self.tool_payload(&chat, &messages);
        match payload {
            Payload::Missing => {
                self.toast_error("The messages are not loaded");
                return;
            }
            Payload::Failed => {
                self.toast_error(format!("Could not download the media for {name}"));
                return;
            }
            Payload::Ready { .. } | Payload::Waiting(_) => {}
        }
        self.dialog = None;
        self.forward_search.clear();
        self.forward_reviewing = false;
        self.tool_prompt.clear();
        self.tool_pick = None;
        self.selection = None;
        match payload {
            Payload::Ready { text, files } => {
                self.backend.send(Command::SendToTool {
                    tool,
                    chat,
                    text,
                    prompt,
                    files,
                });
                self.toast(format!("Sending to {name}…"));
            }
            Payload::Waiting(ids) => {
                for message in ids {
                    self.actions.push(Action::Download {
                        manual: true,
                        card: None,
                        chat: chat.clone(),
                        message,
                    });
                }
                self.tool_jobs.push(ToolJob {
                    account: self.account().id.clone(),
                    tool,
                    chat,
                    messages,
                    prompt,
                });
                self.toast(format!("Downloading media for {name}…"));
            }
            Payload::Missing | Payload::Failed => {}
        }
    }

    /// The Forward dialog's prompt as a tool receives it: trimmed, and `None`
    /// when blank, so a send without one is what it was before prompts.
    fn tool_prompt_to_send(&self) -> Option<String> {
        let prompt = self.tool_prompt.trim();
        (!prompt.is_empty()).then(|| prompt.to_owned())
    }

    /// The size of the media a send would carry.
    fn tool_media_size(&self, chat: &str, messages: &[String]) -> u64 {
        self.conversations.get(chat).map_or(0, |conversation| {
            conversation
                .messages
                .iter()
                .filter(|message| messages.contains(&message.id))
                .filter_map(|message| sent_media(&message.content))
                .map(|media| media.size)
                .sum()
        })
    }

    /// Sends the waiting jobs whose downloads have all finished. Called for
    /// each finished download, while `account()` is the download's own
    /// account.
    pub(super) fn tool_job_step(&mut self) {
        let account = self.account().id.clone();
        // A lock taken while the media downloaded stops the sends.
        if self.app_lock.is_locked() {
            self.tool_jobs.retain(|job| job.account != account);
            return;
        }
        let jobs = std::mem::take(&mut self.tool_jobs);
        for job in jobs {
            if job.account != account {
                self.tool_jobs.push(job);
                continue;
            }
            let name = job.tool.name.trim().to_owned();
            match self.tool_payload(&job.chat, &job.messages) {
                Payload::Waiting(_) => self.tool_jobs.push(job),
                Payload::Ready { text, files } => {
                    self.backend.send(Command::SendToTool {
                        tool: job.tool,
                        chat: job.chat,
                        text,
                        prompt: job.prompt,
                        files,
                    });
                    self.toast(format!("Sending to {name}…"));
                }
                Payload::Failed | Payload::Missing => {
                    self.toast_error(format!("Could not download the media for {name}"));
                }
            }
        }
    }

    /// A tool's answer becomes a draft in its chat, unless text is already
    /// there: then it goes to the clipboard. It is never sent.
    pub(super) fn tool_answered(
        &mut self,
        tool: String,
        chat: ChatId,
        result: Result<Option<String>, String>,
    ) {
        let text = match result {
            Err(error) => return self.toast_error(error),
            Ok(None) => return self.toast(format!("Sent to {tool}")),
            Ok(Some(text)) => text,
        };
        let free = if !self.events_hidden && self.open_chat.as_deref() == Some(chat.as_str()) {
            self.composer.trim().is_empty() && self.editing.is_none()
        } else {
            self.drafts
                .get(&chat)
                .is_none_or(|draft| draft.trim().is_empty())
        };
        if free {
            self.restore_text(&chat, text);
            self.toast(format!(
                "{tool} answered. The answer is in the message box."
            ));
        } else {
            self.actions.push(Action::CopyText(text));
            self.toast(format!(
                "{tool} answered. The message box is in use, so the answer was copied."
            ));
        }
    }
}

/// The media a tool receives as a file: pictures, videos and GIFs, audio
/// and voice messages. Documents and stickers stay in the transcript.
fn sent_media(content: &Content) -> Option<&crate::model::Media> {
    match content {
        Content::Image { media, .. }
        | Content::Video { media, .. }
        | Content::Audio { media, .. } => Some(media),
        _ => None,
    }
}
