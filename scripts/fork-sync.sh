#!/usr/bin/env bash
# Rebuilds the release branch of parlo on top of crmne/zapfast.
#
#   main                     mirror of upstream/main, never committed to.
#                            On GitHub it is protected by a ruleset and only
#                            the "Mirror main" workflow updates it; this
#                            script resets it locally but never pushes it.
#   fork/tooling             this script and its self-test, FORK.md, the two
#                            workflows, the agents' release skill
#   feat/*                   one branch per feature, rebased onto main
#   release                  rebuilt every time: main + merge of every branch above
#   fork/rerere              shared git rerere cache (conflict resolutions),
#                            so the workflow replays what was resolved by hand
#
# Usage: scripts/fork-sync.sh [--push] [--no-test]    every stage, in order
#        scripts/fork-sync.sh --stage <name>          one stage
#
# The stages, which the "Build release" workflow runs as one step each, in
# this order and in the same clone, so a failure names what failed:
#
#   rebase   fetch, then rebase every branch onto upstream's main
#   merge    rebuild release from main and the branches
#   clippy   lint release
#   test     test release
#   push     push release and the branches, and upload the rerere cache
#
# Stops at the first conflict rerere cannot resolve: resolve it,
# `git rebase --continue`, rerun with --push (which also uploads the resolution).
set -euo pipefail

# Rebasing fork/tooling rewrites this file while bash is still reading it:
# run from a copy.
if [ -z "${FORK_SYNC_COPY:-}" ]; then
  copy=$(mktemp)
  cp "$0" "$copy"
  FORK_SYNC_COPY="$copy" exec bash "$copy" "$@"
fi
trap 'rm -f "$FORK_SYNC_COPY"' EXIT

BRANCHES=(fork/tooling feat/mcp feat/remote-transcription feat/external-tools)
RERERE_BRANCH=fork/rerere
PUSH=0
TEST=1
STAGE=
while [ $# -gt 0 ]; do
  case "$1" in
    --push) PUSH=1 ;;
    --no-test) TEST=0 ;;
    --stage)
      STAGE=${2:-}
      shift
      ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done
case "$STAGE" in
  "" | rebase | merge | clippy | test) ;;
  push) PUSH=1 ;;
  *) echo "unknown stage: $STAGE (rebase, merge, clippy, test, push)" >&2; exit 2 ;;
esac

# Whether this run includes a stage: all of them without --stage.
stage() {
  [ -z "$STAGE" ] || [ "$STAGE" = "$1" ] || return 1
  echo "== $1"
}
# clippy, test and push work on the release that the merge stage left checked out.
on_release() {
  [ "$(git branch --show-current)" = release ] && return 0
  echo "stage $1 needs the release branch that the merge stage builds; run the stages in order." >&2
  exit 1
}

cd "$(git rev-parse --show-toplevel)"
if [ -n "$(git status --porcelain)" ]; then
  echo "working tree is dirty; commit or stash first" >&2
  exit 1
fi

git config rerere.enabled true
git config rerere.autoupdate true
RR="$(cd "$(git rev-parse --git-common-dir)" && pwd)/rr-cache"
mkdir -p "$RR"

unmerged() { [ -n "$(git diff --name-only --diff-filter=U)" ]; }

# Rebase onto main; when rerere resolved every conflict, carry on by itself.
rebase_onto_main() {
  git rebase main && return 0
  local tries=0
  while [ "$tries" -lt 100 ]; do
    tries=$((tries + 1))
    unmerged && return 1
    [ -d "$(git rev-parse --git-path rebase-merge)" ] ||
      [ -d "$(git rev-parse --git-path rebase-apply)" ] || return 0
    echo "rerere resolved the conflict; continuing"
    GIT_EDITOR=true git rebase --continue && return 0
  done
  return 1
}

# Called as `save_rerere || ...`, which turns errexit off in here: every step
# that can fail returns by hand. An empty commit id in particular must never
# reach the push, where `:refs/heads/x` deletes the branch.
save_rerere() {
  local idx tree parent="" commit
  idx=$(mktemp -u)
  GIT_INDEX_FILE="$idx" git --work-tree="$RR" add -A -- "$RR" || return 1
  tree=$(GIT_INDEX_FILE="$idx" git write-tree) || return 1
  rm -f "$idx"
  if git rev-parse -q --verify "origin/$RERERE_BRANCH" >/dev/null; then
    parent="origin/$RERERE_BRANCH"
    [ "$(git rev-parse "$parent^{tree}")" = "$tree" ] && return 0
  fi
  commit=$(git commit-tree "$tree" ${parent:+-p "$parent"} -m "rerere cache") || return 1
  [ -n "$commit" ] || return 1
  git push origin "$commit:refs/heads/$RERERE_BRANCH" || return 1
  echo "rerere: cache uploaded to $RERERE_BRANCH"
}

if stage rebase; then
  git remote get-url upstream >/dev/null 2>&1 ||
    git remote add upstream https://github.com/crmne/zapfast.git
  git remote set-url --push upstream no-push

  # The only fetch of origin: the push stage leases against what it saw.
  git fetch upstream main
  git fetch origin

  # A local branch that was never at origin's tip is stale: rebasing and
  # force-pushing it would discard what someone else pushed. Stop before the
  # long build, not after it.
  for branch in "${BRANCHES[@]}"; do
    tip=$(git rev-parse -q --verify "refs/remotes/origin/$branch") || continue
    git rev-parse -q --verify "refs/heads/$branch" >/dev/null || continue
    seen=0
    for at in $(git rev-parse "refs/heads/$branch") $(git reflog show --format=%H "refs/heads/$branch"); do
      if git merge-base --is-ancestor "$tip" "$at"; then seen=1; break; fi
    done
    if [ "$seen" = 0 ]; then
      echo "local $branch never included origin/$branch ($(git rev-parse --short "$tip")): it is stale." >&2
      echo "Take origin's version first: git checkout $branch && git reset --hard origin/$branch" >&2
      echo "(or rebase your local commit onto it), then rerun." >&2
      exit 1
    fi
  done

  # Bring in resolutions recorded elsewhere (another machine, a previous run).
  if git rev-parse -q --verify "origin/$RERERE_BRANCH" >/dev/null; then
    git archive "origin/$RERERE_BRANCH" | tar -x -C "$RR"
    echo "rerere: $(find "$RR" -mindepth 1 -maxdepth 1 -type d | wc -l) recorded resolution(s) loaded"
  fi

  # -B names upstream/main outright: a fresh clone has no local main, and a bare
  # `checkout main` is ambiguous between origin/main and upstream/main.
  git checkout -q -B main upstream/main
  echo "main -> $(git log -1 --format='%h %s')"
  if [ "$(git rev-parse origin/main)" != "$(git rev-parse upstream/main)" ]; then
    echo "warning: origin/main ($(git rev-parse --short origin/main)) is not upstream/main yet;" >&2
    echo "         run the 'Mirror main' workflow so GitHub's main matches this release." >&2
  fi

  for branch in "${BRANCHES[@]}"; do
    git checkout -q "$branch"
    if ! rebase_onto_main; then
      echo
      echo "conflict rebasing $branch onto main ($(git rev-parse --short main))." >&2
      echo "Resolve it, run 'git rebase --continue', then rerun this script with --push;" >&2
      echo "the resolution is saved to $RERERE_BRANCH and replayed from then on." >&2
      exit 1
    fi
    echo "$branch rebased: $(git rev-list --count main..HEAD) commit(s) ahead"
  done
  # Back to the branch this script lives on: the next stage is run from it.
  git checkout -q fork/tooling
fi

if stage merge; then
  git checkout -q -B release main
  for branch in "${BRANCHES[@]}"; do
    if ! git merge --no-ff -q -m "Merge $branch into release" "$branch"; then
      if unmerged; then
        echo "conflict merging $branch into release; resolve, commit, rerun with --push." >&2
        exit 1
      fi
      echo "rerere resolved the merge of $branch"
      git commit -q --no-edit
    fi
  done

  # Cargo.lock may need entries for crates that two features add together.
  cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null
  if [ -n "$(git status --porcelain Cargo.lock)" ]; then
    git commit -q -m "Refresh Cargo.lock for the combined features" Cargo.lock
  fi
  echo "release -> $(git log -1 --format='%h') ($(git rev-list --count main..release) commits over upstream)"
fi

if [ "$TEST" = 1 ] && stage clippy; then
  on_release clippy
  cargo clippy --locked --all-targets -- -D warnings
fi

if [ "$TEST" = 1 ] && stage test; then
  on_release test
  cargo test --locked --all-targets
fi

if [ "$PUSH" = 1 ] && stage push; then
  on_release push
  # main is not pushed: the ruleset rejects it, the workflow owns it.
  # The lease covers a push made since the rebase stage fetched; the staleness
  # check there covers one made before it. Not --force-if-includes: a fresh
  # clone has no reflog for origin's branches, and git then refuses every
  # rebased branch.
  # --atomic: one refused branch refuses them all, so release never lands
  # beside a branch it was not built from.
  git push --atomic --force-with-lease origin release "${BRANCHES[@]}"
  echo "pushed release $(git rev-parse --short release) and ${#BRANCHES[@]} branches"
  # release is out by now: a cache that did not upload is a warning, and the
  # next push uploads it.
  save_rerere || echo "warning: the rerere cache was not uploaded to $RERERE_BRANCH" >&2
fi
