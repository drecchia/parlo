#!/usr/bin/env bash
# Self-test of scripts/fork-sync.sh against scratch repositories: nothing here
# touches the real fork. It takes about half a minute and needs git and cargo.
#
# The workflow runs it before every sync, and it is the check to run after
# changing fork-sync.sh. The cases are the ways the sync has broken:
#
#   1. a fresh clone, as the workflow has it: no local main, no local feat/*
#   2. upstream moved: every branch is rebased and force-pushed
#   3. a second machine with a stale branch must stop before it pushes
#   4. a commit amended on an up-to-date clone must go through
#   5. the stages one by one, as the "Build release" workflow runs them
#   6. a pull request squash-merged as `fixup! <subject>` folds into its
#      branch's one commit
#   7. one squash-merged under another title stops the sync, pushing nothing
#
# Usage: scripts/fork-sync-selftest.sh
set -euo pipefail

script=$(cd "$(dirname "$0")" && pwd)/fork-sync.sh
branches=$(sed -n 's/^BRANCHES=(\(.*\))$/\1/p' "$script")
[ -n "$branches" ] || { echo "selftest: no BRANCHES=(...) line in fork-sync.sh" >&2; exit 1; }

root=$(mktemp -d)
cd "$root"
export GIT_AUTHOR_NAME=selftest GIT_AUTHOR_EMAIL=selftest@invalid
export GIT_COMMITTER_NAME=selftest GIT_COMMITTER_EMAIL=selftest@invalid
failed=0
check() { # check <what> <command...>
  local what=$1
  shift
  if "$@" >/dev/null 2>&1; then echo "ok    $what"; else echo "FAIL  $what"; failed=1; fi
}
origin() { git --git-dir="$root/origin.git" "$@"; }

# "Upstream": a crate with nothing in it, so `cargo update` has work it can do offline.
git init -q -b main seed
(
  cd seed
  printf '[package]\nname = "selftest"\nversion = "0.1.0"\nedition = "2021"\n' > Cargo.toml
  mkdir src && : > src/lib.rs
  cargo generate-lockfile --offline -q
  git add -A && git commit -q -m base
)
git clone -q --bare seed up.git
git clone -q --bare seed origin.git

# The fork: the script on fork/tooling, one commit on each other branch.
git clone -q origin.git setup
(
  cd setup
  for branch in $branches; do
    git checkout -q -b "$branch" main
    if [ "$branch" = fork/tooling ]; then
      mkdir scripts && cp "$script" scripts/fork-sync.sh
    else
      echo "$branch" > "$(basename "$branch").txt"
    fi
    git add -A && git commit -q -m "$branch"
  done
  git checkout -q -b release main
  # shellcheck disable=SC2086
  git push -q origin release $branches
)
feature=$(for branch in $branches; do [ "$branch" = fork/tooling ] || { echo "$branch"; break; }; done)
file=$(basename "$feature").txt

clone() { # as actions/checkout leaves it: on fork/tooling, with no local main
  git clone -q origin.git "$1"
  git -C "$1" checkout -q fork/tooling
  git -C "$1" branch -q -D main
  git -C "$1" remote add upstream "$root/up.git"
}
sync() { (cd "$1" && bash scripts/fork-sync.sh --push --no-test) > "$root/$1.log" 2>&1; }
all_in_release() {
  for branch in $branches; do origin merge-base --is-ancestor "$branch" release || return 1; done
}
amend() { # amend <clone>: change the feature's commit, as a fixup would
  git -C "$1" checkout -q "$feature"
  echo "$1" >> "$1/$file"
  git -C "$1" commit -q -a --amend -m "$feature"
  git -C "$1" checkout -q fork/tooling
}

clone one
check "1. a fresh clone syncs and pushes" sync one
check "1. release holds every branch" all_in_release

clone two # a second machine, level with origin for now
git -C two checkout -q "$feature" && git -C two checkout -q fork/tooling

(cd seed && echo moved > moved.txt && git add -A && git commit -q -m "upstream moves" && git push -q ../up.git main)
clone three
check "2. upstream moved: a fresh clone syncs and pushes" sync three
check "2. the branches sit on the new main" origin merge-base --is-ancestor "$(git --git-dir=up.git rev-parse main)" "$feature"
check "2. release holds every branch" all_in_release

before=$(origin rev-parse "$feature")
amend two
check "3. a stale clone is refused" sh -c "! (cd two && bash scripts/fork-sync.sh --push --no-test) > '$root/two.log' 2>&1"
check "3. and is told why" grep -q "is stale" two.log
check "3. origin keeps its branch" test "$before" = "$(origin rev-parse "$feature")"

amend three
check "4. an amended commit on a current clone is pushed" sync three
check "4. origin has the amended commit" test "$before" != "$(origin rev-parse "$feature")"

(cd seed && echo again > again.txt && git add -A && git commit -q -m "upstream moves again" && git push -q ../up.git main)
clone four
staged() { (cd four && for name in "$@"; do bash scripts/fork-sync.sh --stage "$name"; done) >> "$root/four.log" 2>&1; }
check "5. the push stage refuses to run before the merge stage" sh -c "! (cd four && bash scripts/fork-sync.sh --stage push) > '$root/four.log' 2>&1"
check "5. rebase, merge and push run as separate stages" staged rebase merge push
check "5. the branches sit on the newest main" origin merge-base --is-ancestor "$(git --git-dir=up.git rev-parse main)" "$feature"
check "5. release holds every branch" all_in_release

# A pull request squash-merged into the feature under a fixup title.
git clone -q origin.git pr
(
  cd pr
  git checkout -q "$feature"
  echo "from a pull request" >> "$file"
  git commit -q -a -m "fixup! $feature"
  git push -q origin "$feature"
)
clone five
check "6. a branch with a squash-merged fixup syncs and pushes" sync five
# Counted from upstream's main: the script never pushes main, the mirror does.
check "6. the fixup folded: the branch is one commit again" test "$(origin rev-list --count "$(git --git-dir=up.git rev-parse main)..$feature")" = 1
check "6. that one commit keeps the branch's subject" test "$(origin log -1 --format=%s "$feature")" = "$feature"
check "6. and holds the pull request's change" sh -c "git --git-dir='$root/origin.git' show '$feature:$file' | grep -q 'from a pull request'"

# A pull request squash-merged under its own title, not a fixup.
(
  cd pr
  git fetch -q origin
  git checkout -q -B "$feature" "origin/$feature"
  echo "a plain title" >> "$file"
  git commit -q -a -m "Add something"
  git push -q origin "$feature"
)
before=$(origin rev-parse release)
clone six
check "7. a branch of two commits stops the sync" sh -c "! (cd six && bash scripts/fork-sync.sh --push --no-test) > '$root/six.log' 2>&1"
check "7. and says which title to use" grep -q "squash-merge its pull requests as 'fixup! $feature'" six.log
check "7. release is not pushed" test "$before" = "$(origin rev-parse release)"

if [ "$failed" = 1 ]; then
  echo "selftest: FAILED; clones and logs are in $root" >&2
  exit 1
fi
cd / && rm -rf "$root"
echo "selftest: fork-sync.sh passed"
