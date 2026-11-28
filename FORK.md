# parlo

Downstream fork of [crmne/zapfast](https://github.com/crmne/zapfast) (MIT).
It carries features the upstream declined, kept as separate branches rebased
onto each upstream release.

| Branch | Content | Origin |
|---|---|---|
| `main` | Exact mirror of `upstream/main`. Never commit here: a ruleset blocks every push, and only the `Mirror main` workflow updates it. | crmne/zapfast |
| `fork/tooling` | This file, `scripts/fork-sync.sh` with its self-test, the two workflows (`.github/workflows/fork-sync-main.yml`, `fork-release.yml`), and the agents' release skill (`.claude/skills/fork-release/`). | — |
| `fork/rerere` | Shared `git rerere` cache: conflict resolutions the script replays. Written by the script, never by hand. | — |
| `feat/mcp` | `zapfast --mcp`: MCP server over stdio, plus a private socket to the open window. See `docs/MCP.md`. | [cassiomiller10-hash/zapfast-mcp](https://github.com/cassiomiller10-hash/zapfast-mcp) |
| `feat/remote-transcription` | Manual transcription of voice messages through an OpenAI-compatible provider (OpenAI, Grok, Gemini, local). Key in the OS keyring. | [crmne/zapfast#142](https://github.com/crmne/zapfast/pull/142) |
| `feat/external-tools` | Forward dialog sends selected messages (text, images, video, audio) to a webhook or an Orca bot; the answer returns as a draft. See `docs/EXTERNAL-TOOLS.md`. | — |
| `release` | Disposable: `main` + merge of every branch above. Builds come from here. Default branch of the repository. | — |

## Sync with upstream

Two workflows, so that a failure says which half failed:

1. **`Mirror main`** (`.github/workflows/fork-sync-main.yml`, daily at 06:17
   BRT, or *Run workflow*): fast-forwards `main` to `crmne/zapfast@main`,
   pushing with the `SYNC_DEPLOY_KEY` deploy key, the only bypass of the
   ruleset on `main`. It takes seconds and fails only when upstream rewrote
   its history or the key stopped working. When `release` does not contain
   `main` yet, it starts the second workflow.
2. **`Build release`** (`.github/workflows/fork-release.yml`, started by the
   first, or *Run workflow* after a pull request is merged into `fork/tooling`
   or a `feat/*` branch): one step per stage of `scripts/fork-sync.sh`, so
   the red step is the answer:

   | Step | A failure means |
   |---|---|
   | Self-test of the sync script | the script itself is broken |
   | Rebase the branches onto main | a branch conflicts with the new upstream |
   | Merge the branches into release | two branches conflict with each other |
   | Clippy, Tests | a feature's code no longer passes on the new upstream |
   | Push release and the branches | a branch changed during the run: run it again |

   Nothing on GitHub changes before the push step: `release` keeps its last
   good version, and the workflow opens (or comments on) an issue labelled
   `fork-sync` that names the stage.

### Conflicts

Conflict resolutions are shared through `git rerere` and the `fork/rerere`
branch: the script loads it before rebasing, replays every resolution it
knows (and carries on by itself when that covers the whole conflict), and
uploads the cache after a `--push`. A conflict seen for the first time still
needs a person, once:

```sh
git checkout fork/tooling
scripts/fork-sync.sh             # stops at the conflict
# resolve, git add, git rebase --continue
scripts/fork-sync.sh --push      # pushes the branches and the resolution
```

From then on the workflow resolves that conflict on its own.

The script runs every stage in order locally (`scripts/fork-sync.sh`, with
`--no-test` to skip clippy and the tests, `--push` to push). It resets `main`
to `upstream/main` locally but never pushes it, and warns if GitHub's `main`
is behind (run `Mirror main` first).

`release` is force-pushed on every sync. An existing clone catches up with
`git fetch origin && git reset --hard origin/release`.

The script stops at once when a local branch never included its `origin`
tip: that copy is stale, and pushing it would discard what was pushed from
elsewhere. Take origin's version (or rebase onto it) and rerun.

The workflow files run from `release`, the default branch, but `Build
release` checks out `fork/tooling` and runs the script from there. A fix to
the script takes effect on the next run; a fix to a workflow file only after
one build of `release` has passed.

`scripts/fork-sync-selftest.sh` runs the script against scratch repositories
in half a minute: a fresh clone, an upstream that moved, a stale second
machine, an amended commit, the stages one by one. `Build release` runs it
first; run it after any change to the script.

Agents: `.claude/skills/fork-release/SKILL.md` is the release procedure
(`.claude/` is ignored upstream, so the file is added with `git add -f`).

### When upstream rewrites its history

The workflow only fast-forwards, so it fails. Then: in *Settings > Rules >
Rulesets*, set the `main` ruleset to *Disabled*,
`git push --force-with-lease origin upstream/main:main`, re-enable the
ruleset, and run `scripts/fork-sync.sh --push`.

### Deploy key setup (once)

`ssh-keygen -t ed25519 -f parlo-sync -N ""`, then the public key in *Settings >
Deploy keys* with write access, and the private key as the Actions secret
`SYNC_DEPLOY_KEY`. The `main` ruleset lists *Deploy keys* as its only bypass.

## Rules that keep the rebase cheap

- One commit per feature. A change to an existing `feat/*` branch or to
  `fork/tooling` goes through a pull request: a `dev/<topic>` branch from
  that branch, a pull request back into it titled
  `fixup! <the branch's commit subject>`, squash-merged under that title.
  The squash commit is then a fixup, and the next `Build release` folds it
  into the branch's one commit (`git rebase -i --autosquash` in its rebase
  stage). With one commit in the pull request GitHub takes that commit's
  message as the squash title, so give the commit the same `fixup!` title.
- New code goes in new files (`src/mcp/`, `src/transcribe.rs`). Changes to
  upstream files stay at small hook points.
- A new feature means a new `feat/*` branch from `main`, pushed with its
  first commit, and a pull request into `fork/tooling` that adds it to
  `BRANCHES` in the script and to the table and the Risk section here.
- Never push to `upstream`: its push URL is set to `no-push`.

## Risk

These features are outside what the upstream accepts. MCP sends from the
account, remote transcription sends audio to a third party, and external tools
send message text and media to endpoints the user configures. WhatsApp bans
accounts for automated use. Use a number you can afford to lose.
