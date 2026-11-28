# External tools

A fork feature (`feat/external-tools`). Select messages, press **Forward…**,
and pick a tool under **Tools**, above the chats. The next step shows the
tool and an optional **Prompt**; **Send** (or Ctrl+Enter) sends, **Back**
returns to the list. The tool gets the messages; with **Answer** on, its
answer comes back as a draft in the message box of the chat the messages
came from. ZapFast never sends the answer: you read it, edit it, and send it
yourself.

Tools are listed in **Settings → External tools**. Each has a kind, a name,
a target and the **Answer** switch. A row without a name or target is kept
but not offered.

The text and media of the messages you pick leave ZapFast for the tool.
Every send starts with your click; nothing is sent on a schedule or by
itself.

## What a tool receives

The text is what Ctrl+C copies from a selection: one line per message,
`[time] Name: text`, with markers such as `[photo]` or `[voice message, 0:12]`.

Pictures, videos, GIFs, audio and voice messages also go as files. ZapFast
downloads any that are not downloaded yet before it sends. Documents and
stickers stay as their markers in the text.

The **Prompt**, up to 4,000 characters, tells the tool what to do with these
messages, such as "Summarize this negotiation and open a ticket". The prompt
goes in a field of its own, apart from the messages, and a tool that does not
know it can ignore it. Left blank, the tool gets exactly what it got before
the field existed. **Back** keeps what you typed; the field starts empty each
time the dialog opens.

## Webhook

The target is a URL: HTTPS, or HTTP only to this computer (`localhost`,
`127.0.0.1`, `::1`). Put any key in the URL itself, such as
`https://example.com/hook?key=…`. ZapFast uses the Settings proxy, sends no
cookies and follows no redirect.

- Text only: `POST` with `Content-Type: application/json` and the body
  `{"text": "…"}`, or `{"text": "…", "prompt": "…"}` with a prompt.
- With media: `POST` as `multipart/form-data`, a `text` field, a `prompt`
  field when there is a prompt, and one `file` part per file, named `1.jpg`,
  `2.ogg`… in the messages' order, with the file's MIME type.

The answer is the response body: the `text` field of a JSON body, or a plain
text body as it is. An empty answer, or a JSON body without `text`, means
there is nothing to put in the message box. Answers over 64 KiB are refused.
The tool has 5 minutes to answer. Any status other than 2xx shows as an
error naming only the status.

## Orca bot

The target is the name of a bot of the Orca app on this
computer, as `orca bot list` shows it. ZapFast runs the Orca CLI
(`ORCA_CLI_COMMAND`, else `orca`, or `orca-ide` on Linux):

1. `orca orchestration run-create` makes a Run, whose inbox gets the answer.
   Each send makes one, and it stays in Orca's list of Runs.
2. `orca orchestration send --run <run> --to bot:<name> --type question
   --chat new` hands the bot the text in a new chat of its own. With a prompt,
   the message is `Prompt:`, the prompt, a blank line, `Forwarded messages:`
   and the text; without one it is the text alone. Media is not attached: the
   message lists the paths of the downloaded files on this computer, and the
   bot sees them only if its own tools can read files.
3. With **Answer** on, `orca orchestration check --run <run> --wait` waits up
   to 5 minutes for the bot's answer. Without it, the answer stays in Orca.

## What is not logged

ZapFast logs no target, message text, file path or answer, and its errors
name only the tool and an HTTP status.
