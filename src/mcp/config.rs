//! What the MCP server may do, read once from the environment.
//!
//! Everything fails closed: an unset or unreadable switch means "no". Read
//! tools are always on; every tool that changes something (a sent message, a
//! sent file, a read mark) needs `ZAPFAST_MCP_ALLOW_WRITE`.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Master switch for every tool that changes something.
pub const ALLOW_WRITE: &str = "ZAPFAST_MCP_ALLOW_WRITE";
/// Narrows `ALLOW_WRITE` for `whatsapp_send_message`. On when writing is on.
pub const ALLOW_SEND: &str = "ZAPFAST_MCP_ALLOW_SEND";
/// Allows `whatsapp_send_file`. Off unless set, even when writing is on.
pub const ALLOW_FILES: &str = "ZAPFAST_MCP_ALLOW_FILES";
/// Directories `whatsapp_send_file` may read from (platform path list).
pub const FILE_ROOTS: &str = "ZAPFAST_MCP_FILE_ROOTS";
/// Starts the server although the app lock is set in the settings.
pub const ALLOW_APP_LOCKED: &str = "ZAPFAST_MCP_ALLOW_APP_LOCKED";

/// The tools that change something, for the permission check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Send,
    SendFile,
    MarkRead,
}

/// A refused write, worded for the agent that asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied(pub String);

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Permissions {
    write: bool,
    send: bool,
    files: bool,
}

impl Permissions {
    /// Read tools only.
    pub fn read_only() -> Self {
        Self::default()
    }

    /// Everything the environment allows, for tests.
    #[cfg(test)]
    pub fn all() -> Self {
        Self {
            write: true,
            send: true,
            files: true,
        }
    }

    pub fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> Self {
        let flag = |name: &str| lookup(name).map(|value| truthy(&value));
        let write = flag(ALLOW_WRITE).unwrap_or(false);
        Self {
            write,
            send: write && flag(ALLOW_SEND).unwrap_or(true),
            files: write && flag(ALLOW_FILES).unwrap_or(false),
        }
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.check(capability).is_ok()
    }

    /// Whether `capability` may run, or what to tell the agent.
    pub fn check(&self, capability: Capability) -> Result<(), Denied> {
        if !self.write {
            return Err(Denied(format!(
                "Write tools are disabled: this ZapFast MCP server is read-only. The owner can \
                 enable them by starting it with {ALLOW_WRITE}=true."
            )));
        }
        match capability {
            Capability::Send if !self.send => Err(Denied(format!(
                "Sending messages is disabled by {ALLOW_SEND}=false."
            ))),
            Capability::SendFile if !self.files => Err(Denied(format!(
                "Sending files is disabled. The owner can enable it with {ALLOW_FILES}=true \
                 (optionally limiting folders with {FILE_ROOTS})."
            ))),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub permissions: Permissions,
    /// Allowed folders for `whatsapp_send_file`; empty means any folder that
    /// is not off limits.
    pub file_roots: Vec<PathBuf>,
    /// Whether the server may start while the app lock is set.
    pub allow_app_locked: bool,
    /// How long a tool waits for the backend before giving up.
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            permissions: Permissions::read_only(),
            file_roots: Vec::new(),
            allow_app_locked: false,
            timeout: Duration::from_secs(20),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> Self {
        let roots = lookup(FILE_ROOTS)
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        Self {
            permissions: Permissions::from_lookup(lookup),
            file_roots: roots,
            allow_app_locked: lookup(ALLOW_APP_LOCKED).is_some_and(|value| truthy(&value)),
            ..Self::default()
        }
    }
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Why a file may not be sent.
#[derive(Debug, PartialEq, Eq)]
pub enum FileRefusal {
    Missing,
    NotAFile,
    OutsideRoots,
    OffLimits,
    Empty,
    TooLarge,
}

impl std::fmt::Display for FileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "The file does not exist or cannot be read.",
            Self::NotAFile => "The path is not a regular file.",
            Self::OutsideRoots => {
                "The file is outside the folders this server may send from (ZAPFAST_MCP_FILE_ROOTS)."
            }
            Self::OffLimits => {
                "That location holds credentials or ZapFast's own data and is never sent."
            }
            Self::Empty => "The file is empty.",
            Self::TooLarge => "The file is larger than the 64 MiB this server sends.",
        })
    }
}

/// Folder names that hold credentials, wherever they sit in the path.
const SECRET_DIRECTORIES: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".docker"];

/// Checks `path` before it is handed to the backend and returns the resolved
/// file. `private` lists ZapFast's own folders (session, archive, settings).
pub fn vet_file(
    path: &str,
    roots: &[PathBuf],
    private: &[PathBuf],
) -> Result<PathBuf, FileRefusal> {
    let path = Path::new(path.trim());
    let resolved = path.canonicalize().map_err(|_| FileRefusal::Missing)?;
    let metadata = std::fs::metadata(&resolved).map_err(|_| FileRefusal::Missing)?;
    if !metadata.is_file() {
        return Err(FileRefusal::NotAFile);
    }
    let inside = |folder: &PathBuf| {
        folder
            .canonicalize()
            .is_ok_and(|folder| resolved.starts_with(folder))
    };
    if private.iter().any(inside)
        || resolved.components().any(|part| {
            part.as_os_str()
                .to_str()
                .is_some_and(|name| SECRET_DIRECTORIES.contains(&name))
        })
    {
        return Err(FileRefusal::OffLimits);
    }
    if !roots.is_empty() && !roots.iter().any(inside) {
        return Err(FileRefusal::OutsideRoots);
    }
    match metadata.len() {
        0 => Err(FileRefusal::Empty),
        size if size > crate::model::ATTACHMENT_DOWNLOAD_LIMIT => Err(FileRefusal::TooLarge),
        _ => Ok(resolved),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn nothing_is_writable_by_default() {
        let permissions = Permissions::from_lookup(&env(&[]));
        for capability in [Capability::Send, Capability::SendFile, Capability::MarkRead] {
            let denied = permissions.check(capability).unwrap_err();
            assert!(denied.0.contains(ALLOW_WRITE));
        }
    }

    #[test]
    fn unreadable_values_fail_closed() {
        for value in ["", "false", "0", "maybe", "tru"] {
            let permissions = Permissions::from_lookup(&env(&[(ALLOW_WRITE, value)]));
            assert!(!permissions.allows(Capability::Send), "{value:?}");
        }
    }

    #[test]
    fn write_allows_messages_and_read_marks_but_not_files() {
        let permissions = Permissions::from_lookup(&env(&[(ALLOW_WRITE, "true")]));
        assert!(permissions.allows(Capability::Send));
        assert!(permissions.allows(Capability::MarkRead));
        assert!(
            permissions
                .check(Capability::SendFile)
                .unwrap_err()
                .0
                .contains(ALLOW_FILES)
        );
    }

    #[test]
    fn narrower_switches_cannot_widen_the_master_switch() {
        let permissions = Permissions::from_lookup(&env(&[
            (ALLOW_WRITE, "false"),
            (ALLOW_SEND, "true"),
            (ALLOW_FILES, "true"),
        ]));
        assert_eq!(permissions, Permissions::read_only());
        let permissions = Permissions::from_lookup(&env(&[
            (ALLOW_WRITE, "1"),
            (ALLOW_SEND, "false"),
            (ALLOW_FILES, "yes"),
        ]));
        assert!(!permissions.allows(Capability::Send));
        assert!(permissions.allows(Capability::SendFile));
        assert!(permissions.allows(Capability::MarkRead));
    }

    #[test]
    fn config_reads_roots_and_the_app_lock_opt_in() {
        let roots = std::env::join_paths(["/a", "/b"]).unwrap();
        let roots = roots.to_str().unwrap();
        let config = Config::from_lookup(&env(&[(FILE_ROOTS, roots), (ALLOW_APP_LOCKED, "on")]));
        assert_eq!(
            config.file_roots,
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(config.allow_app_locked);
        assert!(!Config::from_lookup(&env(&[])).allow_app_locked);
    }

    #[test]
    fn vetting_refuses_private_hidden_and_unlisted_files() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let state = dir.join("state");
        let outside = dir.join("outside");
        let ssh = dir.join(".ssh");
        for folder in [&state, &outside, &ssh] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let session = state.join("session.db");
        let note = outside.join("note.txt");
        let key = ssh.join("id_ed25519");
        for file in [&session, &note, &key] {
            std::fs::write(file, b"data").unwrap();
        }
        let private = [state.clone()];
        let roots = [outside.clone()];
        let vet = |path: &Path| vet_file(path.to_str().unwrap(), &roots, &private);
        assert_eq!(vet(&note), Ok(note.clone()));
        assert_eq!(vet(&session), Err(FileRefusal::OffLimits));
        assert_eq!(vet(&key), Err(FileRefusal::OffLimits));
        assert_eq!(vet(&outside), Err(FileRefusal::NotAFile));
        assert_eq!(vet(&dir.join("missing")), Err(FileRefusal::Missing));
        // Without roots, only the off-limits rule applies.
        assert_eq!(vet_file(note.to_str().unwrap(), &[], &private), Ok(note));
        std::fs::write(outside.join("empty"), b"").unwrap();
        assert_eq!(vet(&outside.join("empty")), Err(FileRefusal::Empty));
    }

    #[test]
    fn vetting_resolves_links_before_judging_them() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("archive.db"), b"data").unwrap();
        #[cfg(unix)]
        {
            let link = dir.join("innocent.txt");
            std::os::unix::fs::symlink(state.join("archive.db"), &link).unwrap();
            assert_eq!(
                vet_file(link.to_str().unwrap(), &[], &[state]),
                Err(FileRefusal::OffLimits)
            );
        }
    }
}
