---
name: fork-release
description: Release procedure of the parlo fork. Use to publish a feature branch, rebuild or build `release`, sync with upstream, add a `feat/*` branch, change `scripts/fork-sync.sh` or the Mirror main / Build release workflows, or when one of their runs failed.
---

# Fork release

`FORK.md` on `fork/tooling` is the source of truth for the branch model and the two workflows. Read it first. This skill is the order of work and the traps `FORK.md` does not spell out.

The remote decides: every step starts from `git fetch origin`, and `origin/<branch>` wins over any local branch you did not just change yourself. The development machine is ephemeral: work exists once it is pushed to its branch on GitHub.

## Where a change goes

| Change | Branch |
|---|---|
| Code or docs of one feature | a pull request into that feature's `feat/<name>` |
| `FORK.md`, `scripts/fork-sync*.sh`, `.github/workflows/fork-*.yml`, this skill | a pull request into `fork/tooling` |
| A new feature | a new `feat/<name>` from `main`, pushed with its first commit, plus a pull request into `fork/tooling` for `BRANCHES` |
| Process summary in Portuguese | issue #1 (`gh issue edit 1 -R drecchia/parlo`) |
| Anything | never `main`, never `release`; no direct push to an existing `feat/*` or `fork/tooling` |

## Traps

- **GitHub builds `release`, not this machine.** `Build release` runs the stages of `scripts/fork-sync.sh` on Linux. Run the script locally only to resolve a conflict (see `FORK.md`), and on Windows only under Git Bash (`"/c/Program Files/Git/bin/bash.exe" scripts/fork-sync.sh`).
- **Two sources.** Workflow files are read from `release`; `Build release` checks out `fork/tooling` and runs the script from there. A script fix works on the next run. A workflow-file fix works only after one green build.
- **One build at a time.** Runs queue. While one is in progress, merge no pull request into `fork/tooling` or `feat/*`: the build's push step is leased and will be refused.
- **Runs you did not start belong to the owner.** Leave them queued or running.
- **`gh` needs `-R drecchia/parlo`.** With the `upstream` remote present it targets `crmne/zapfast`.
- **Run the script from the checked-out `fork/tooling`,** never from a copy taken out of another commit.

## Publish a change

Every change to an existing `feat/*` branch or to `fork/tooling` is a pull request that the owner reviews and merges.

1. Start from GitHub's copy: `git fetch origin && git switch -c dev/<topic> origin/<target>`, where `<target>` is the `feat/*` branch or `fork/tooling`.
2. Take the target's subject: `subject=$(git log -1 --format=%s origin/<target>)`. Commit your work as `git commit -m "fixup! $subject"`; further commits on the same pull request can have any title.
3. Changed `scripts/fork-sync.sh` only: run `scripts/fork-sync-selftest.sh`.
   Done when: it prints `selftest: fork-sync.sh passed`.
4. Push and open the pull request:
   `git push -u origin dev/<topic>` and `gh pr create -R drecchia/parlo --base <target> --head dev/<topic> --title "fixup! $subject" --body "<what, why, how it was verified>"`.
   Done when: the pull request exists and its title is exactly `fixup! ` plus the target's subject.
5. Stop and hand the pull request to the owner. The owner merges with **Squash and merge**, keeping the `fixup!` title, and deletes the `dev/` branch.
6. After the merge, start one build: `gh workflow run fork-release.yml -R drecchia/parlo`. Wait for it with one `gh run watch <id> -R drecchia/parlo --exit-status --interval 60`. A cold build takes about half an hour. Its rebase stage folds the fixup, so the target is one commit again.
   Done when: the run is green, `git fetch origin` then `git rev-list --count origin/main..origin/<target>` is `1`, and `git merge-base --is-ancestor origin/<branch> origin/release` succeeds for `main` and every branch in `BRANCHES`.
7. Update issue #1 when the branch list or the process changed.

A new feature: `git switch -c feat/<name> origin/main`, one commit, `git push -u origin feat/<name>`; then a pull request into `fork/tooling` (steps 1 to 5) that adds `feat/<name>` to `BRANCHES` and to the table and the Risk section of `FORK.md`.

## When a run fails

`Mirror main` red: upstream rewrote its history, or the deploy key. `FORK.md` has the manual sync.

`Build release` red: the red step names the stage, and the `fork-sync` issue repeats it with the end of the log.

| Red step | Do |
|---|---|
| Self-test, or anything before it | Reproduce with `scripts/fork-sync-selftest.sh`, fix on `fork/tooling`, publish |
| Rebase, Merge | Resolve once by hand as `FORK.md` describes; the resolution is shared through `fork/rerere` |
| Clippy, Tests | Fix on the `feat/*` branch that owns the file, publish |
| Push | A branch moved during the run: start the build again |

Stop and report after one failed run of the same cause. `release` keeps its last good version in the meantime.

## Build the binary

Build what GitHub validated, from a detached checkout of the exact commit:

```sh
git fetch origin && git checkout --detach origin/release
cargo build --release --locked      # target/release/zapfast(.exe)
```

Close any ZapFast started from `target/release` first: on Windows the linker cannot replace an open `.exe`.

Windows needs a full Perl and CMake for the bundled OpenSSL and Opus. Git's own `perl` lacks modules, so name them:

```sh
export PERL='C:\Strawberry\perl\bin\perl.exe'     # winget install StrawberryPerl.StrawberryPerl
export CMAKE='<Visual Studio Build Tools>\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
```
