# ZapFast MCP server

`zapfast --mcp` serves your linked WhatsApp account to MCP clients (Codex,
Claude, Orca, and others) over standard input and output. It is another
interface to the same backend the window uses. It does not reimplement the
WhatsApp protocol, and it uses no browser, no Evolution API, and no network
port.

```
MCP client (Codex / Claude / Orca)
        |  MCP over stdio
   zapfast --mcp          src/mcp/*
        |  Command / Event
   ZapFast Backend        src/backend.rs, src/backend/worker.rs
        |
   whatsapp-rust  ->  WhatsApp
```

## Architecture

- `src/mcp/server.rs`: the tools (`rmcp`, the official Rust SDK).
- `src/mcp/bridge.rs`: one `AccountHandle` per account. It owns the account's
  `Backend`, drains its `Event`s into a small mirror (chat list, contacts, link
  state), and lets a tool send a `Command` and wait for the matching `Event`.
  Events carry no request id, so each account serves one tool at a time.
- `src/mcp/config.rs`: permissions from the environment, and file vetting.
- `src/mcp/phone.rs`: number and chat id parsing.
- `src/mcp/types.rs`: parameters and answers.

Reads use `Command::LoadChat`, `SearchMessages` and `SearchChatMessages`, so
they see exactly what the window sees: the encrypted archive on this computer.
Sends use `Command::SendText` and `SendFiles`, read marks `Command::MarkRead`,
and a number nobody has written to yet is checked with `Command::NewContact`
(WhatsApp's registration check) before anything is sent.

There are two ways to run it, and both keep one process as the only owner of
`session.db` and `archive.db` (the single-instance lock):

- **With the window open (macOS and Linux).** The window hosts the server on a
  private Unix socket (`mcp.sock` in the runtime folder, mode 0600, no TCP
  port). `zapfast --mcp` notices the window and only relays standard input and
  output to that socket. Turn it on in Settings, "Share with AI agents (MCP)"
  (setting `mcp_server`), then restart ZapFast. Several clients can connect at
  once. It stays off while the app lock is set.
- **Standalone, without a window.** `zapfast --mcp` opens the account itself.
  Close the window first, since it would hold the session.

In the window, what a client may do is the smaller of what its environment asks
for and the window's own setting "Let AI agents send messages"
(`mcp_allow_write`, off by default). With that setting off, every client is
read-only whatever `ZAPFAST_MCP_ALLOW_WRITE` says.

## Build

```sh
cargo build --release --locked
# binary: target/release/zapfast
```

The build needs what ZapFast already needs (a C++ compiler and cmake; see the
getting started guide). `rmcp` is the only new dependency.

## Link the account first

Linking happens in the window only. The MCP server never shows a QR code and
never starts pairing.

1. Run `zapfast` (no flags), link your phone (Settings, Linked devices), and
   wait for the first history sync.
2. Close ZapFast completely, including the tray item.
3. Start the MCP server from your client.

An account with no linked session is not connected by the server; its tools
answer `not_linked`.

## Run

```sh
zapfast --mcp                  # the account the window used last
zapfast --mcp --mcp-account 2  # a specific account (repeat for several)
```

Standard output carries only MCP messages. Logs go to standard error and never
contain message text, numbers, keys, or QR payloads (the same redaction as the
window). The server stops when the client closes standard input, or on Ctrl-C
or SIGTERM, and stops the backend cleanly first.

## Configure a client

Read-only (the default):

```json
{
  "mcpServers": {
    "whatsapp": {
      "command": "/path/to/zapfast",
      "args": ["--mcp"],
      "env": { "ZAPFAST_MCP_ALLOW_WRITE": "false" }
    }
  }
}
```

With sending enabled:

```json
{
  "mcpServers": {
    "whatsapp": {
      "command": "/path/to/zapfast",
      "args": ["--mcp"],
      "env": { "ZAPFAST_MCP_ALLOW_WRITE": "true" }
    }
  }
}
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.whatsapp]
command = "/path/to/zapfast"
args = ["--mcp"]
env = { ZAPFAST_MCP_ALLOW_WRITE = "false" }
```

Claude Code: `claude mcp add whatsapp -- /path/to/zapfast --mcp`.

## Tools

Every tool takes an optional `account_id`; without it the default account
answers. Chats are named by `chat_id` as `whatsapp_list_chats` returns them
(`5562999999999@s.whatsapp.net`, `1203...@g.us`). Where a tool takes `phone`,
digits with the country code are enough (`+55 62 99999-9999` works too).

| Tool | Parameters | Answer |
| --- | --- | --- |
| `whatsapp_status` | none | accounts, link state, enabled write tools |
| `whatsapp_list_chats` | `limit`, `query`, `include_archived` | `chat_id`, `name`, `is_group`, `unread_count`, `last_message_timestamp`, `last_message_preview` |
| `whatsapp_get_messages` | `chat_id` or `phone`, `limit` (30, max 200), `before` | `id`, `chat_id`, `sender`, `sender_name`, `from_me`, `timestamp`, `text`, `message_type`, media metadata, `next_before` |
| `whatsapp_search_messages` | `query`, `chat_id`, `limit`, `from`, `until` | matching messages with `chat_name` |
| `whatsapp_get_contact` | `phone` or `chat_id` | `id`, `phone`, `display_name`, `profile_name`, `saved_name` |
| `whatsapp_send_message` | `chat_id` or `phone`, `text`, `reply_to` | `success`, `message_id`, `status`, `error` |
| `whatsapp_send_file` | `chat_id` or `phone`, `path`, `caption` | `success`, `message_id`, `status`, `error` |
| `whatsapp_mark_read` | `chat_id` or `phone`, `send_receipts` | `success`, `unread_count` |

Notes:

- `before` is the `next_before` of an earlier answer. `from` and `until` take a
  Unix timestamp, an ISO 8601 instant, or a date (`2026-10-07`; as `until` it
  includes that day).
- Search without `chat_id` returns at most 50 hits (a backend limit), narrowed by
  `from` and `until` afterwards. With `chat_id` it returns at most 80.
- Media is described (type, size, dimensions, duration, file name, whether it is
  already downloaded), never returned, and local file paths are not exposed.
- `status` of a send is `sent` once WhatsApp's server accepted it, or `pending`
  if the acknowledgement had not arrived within the timeout (20 s). A failed send
  returns `success: false` with `error_code: send_failed` and the `message_id`.
- `whatsapp_mark_read` sends no read receipts unless `send_receipts` is true.
- Errors come back as tool results with `isError: true`, `success: false`,
  `error`, and `error_code` (`permission_denied`, `invalid_params`, `not_found`,
  `not_linked`, `not_ready`, `offline`, `timeout`, `send_failed`).

## Permissions

Read tools are always on. Everything that changes something is off until the
owner of the server turns it on in its environment:

| Variable | Default | Effect |
| --- | --- | --- |
| `ZAPFAST_MCP_ALLOW_WRITE` | `false` | Master switch for `send_message`, `send_file`, `mark_read`. |
| `ZAPFAST_MCP_ALLOW_SEND` | `true` | With write on, `false` disables `send_message`. |
| `ZAPFAST_MCP_ALLOW_FILES` | `false` | With write on, `true` enables `send_file`. |
| `ZAPFAST_MCP_FILE_ROOTS` | unset | Folders `send_file` may read from (path list, `:` or `;`). |
| `ZAPFAST_MCP_ALLOW_APP_LOCKED` | `false` | Let the server start although the app lock is set. |

Accepted true values: `1`, `true`, `yes`, `on`. Anything else is false. A narrower
switch can never widen the master switch. A denied call returns
`permission_denied` and names the variable; the backend is never contacted.

## Security

- **Message content is untrusted.** Other people write it, and it can contain
  prompt injection. The server returns it as data, marks every such answer with
  `untrusted_content_notice`, and never interprets it: nothing in the server
  runs a shell or changes configuration, and no tool accepts a command. The
  protection that remains is the agent's, so enable write tools only for agents
  and prompts you trust, and prefer a client that asks you to approve each send.
- **App lock.** The app lock is a screen lock in the window. The server cannot
  show it, so with the lock set the server refuses to start unless
  `ZAPFAST_MCP_ALLOW_APP_LOCKED=true`. Existing protection is not lowered
  silently.
- **Locked chats.** Chats the phone locked are never listed, read, searched, or
  written to.
- **Secrets.** Session keys, the archive's SQLCipher key, the OS keyring, QR
  codes and pairing codes are never returned or logged. The archive is opened
  only through the backend; the server never reads `archive.db` or `session.db`
  itself.
- **File sending.** Off by default. Files inside ZapFast's own config and state
  folders, and anything under a `.ssh`, `.gnupg`, `.aws`, `.kube` or `.docker`
  folder, are refused after links are resolved. Set `ZAPFAST_MCP_FILE_ROOTS` to
  allow only specific folders. Files over 64 MiB are refused.
- **No network surface.** No port, no HTTP. Only the process that started the
  server can talk to it.
- **One owner of the session.** The single-instance lock keeps two processes
  from holding the same WhatsApp session.

## Limitations

- Sharing with the open window needs a Unix socket, so it is not available on
  Windows; there `--mcp` works only without the window.
- With the window open, tools of one account still run one at a time, and
  something you do in the window at the same moment (opening that chat, or an
  error toast) can be mistaken for the answer to a tool call. Retry if a call
  fails oddly while you are using the window.
- The server serves one account by default (the one the window used last);
  `--mcp-account` selects others.
- Reads cover what is stored locally. History is not fetched from the phone on
  demand, and search only sees stored messages.
- Opening a chat for the first time also asks WhatsApp for group details and
  subscribes to presence, as the window does.
- Brazilian numbers with and without the extra 9 are not unified: use the
  `chat_id` from `whatsapp_list_chats` when a chat already exists.
- No media download, reactions, edits, deletion, groups management, or polls yet.
- The server looks for backend events every 20 ms, because the backend's waker
  is tied to the window.
- Events carry no request id, so tools of one account run one at a time.

## Staying current with upstream

```sh
git remote add upstream https://github.com/crmne/zapfast.git   # once
git fetch upstream
git checkout main && git merge --ff-only upstream/main
git checkout feat/mcp-server && git rebase main
cargo test --locked --all-targets
```

The changes outside `src/mcp/` are small: `src/main.rs` (the `--mcp` flags and
`run_mcp`), `src/lib.rs` (one `mod`), `Cargo.toml` (`rmcp`, tokio's `signal`
feature) and `Cargo.lock`, so conflicts are rare.
