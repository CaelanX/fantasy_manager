#!/usr/bin/env bash
# Keep the data folder between GitHub Actions runs (.github/workflows/hosted.yml): one encrypted
# tarball, data.tar.gz.enc, on the orphan branch $FM_DATA_BRANCH (default fm-data). Each push
# replaces the branch with a single commit, so the repo does not grow run after run.
#
#   deploy/gh-data.sh pull   restore the data folder from the branch (nothing to do before the first push)
#   deploy/gh-data.sh push   save the data folder to the branch
#
# Needs FM_DATA_KEY (a long random passphrase, the same one as the repo secret): the repo is public,
# so the tarball is AES-256 encrypted (openssl, PBKDF2). The HTTP cache (cache.db) and logs are left
# out: the next run rebuilds them.
#
# Seed it once from your PC (Git Bash, in the repo folder, after `git push` works):
#   FM_DATA_KEY='<the FM_DATA_KEY secret>' bash deploy/gh-data.sh push
set -euo pipefail

BRANCH="${FM_DATA_BRANCH:-fm-data}"
DATA_DIR="${FM_DATA_DIR:-data}"
FILE=data.tar.gz.enc
: "${FM_DATA_KEY:?set FM_DATA_KEY to the data passphrase (the FM_DATA_KEY repo secret)}"
export FM_DATA_KEY
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
enc() { openssl enc -aes-256-cbc -pbkdf2 -iter 200000 -salt -pass env:FM_DATA_KEY "$@"; }

case "${1:-}" in
  pull)
    rc=0
    git ls-remote --exit-code --heads origin "$BRANCH" > /dev/null || rc=$?
    if (( rc == 2 )); then        # the branch does not exist (any other failure stops the run)
      echo "no $BRANCH branch yet: starting with an empty data folder"
      mkdir -p "$DATA_DIR"
      exit 0
    elif (( rc != 0 )); then
      echo "cannot reach origin to check $BRANCH (exit $rc)" >&2
      exit 1
    fi
    git fetch --quiet --depth 1 origin "refs/heads/$BRANCH"
    git show "FETCH_HEAD:$FILE" > "$work/$FILE"
    mkdir -p "$DATA_DIR"
    enc -d -in "$work/$FILE" | tar -xz -C "$DATA_DIR"
    echo "data restored from $BRANCH ($(du -sh "$DATA_DIR" | cut -f1))"
    ;;
  push)
    [[ -d "$DATA_DIR" ]] || { echo "no data folder at $DATA_DIR" >&2; exit 1; }
    tar -cz -C "$DATA_DIR" --exclude=./cache.db --exclude='./cache.db-*' --exclude=./logs . \
      | enc -out "$work/$FILE"
    blob=$(git hash-object -w "$work/$FILE")
    tree=$(printf '100644 blob %s\t%s\n' "$blob" "$FILE" | git mktree)
    commit=$(GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-fm-data}" GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-fm-data@users.noreply.github.com}" \
             GIT_COMMITTER_NAME="${GIT_COMMITTER_NAME:-fm-data}" \
             GIT_COMMITTER_EMAIL="${GIT_COMMITTER_EMAIL:-fm-data@users.noreply.github.com}" \
             git commit-tree "$tree" -m "data $(date -u +%Y-%m-%dT%H:%MZ)")
    git push --quiet --force origin "$commit:refs/heads/$BRANCH"
    echo "data saved to $BRANCH ($(du -h "$work/$FILE" | cut -f1) encrypted)"
    ;;
  *)
    echo "usage: $0 pull|push" >&2
    exit 2
    ;;
esac
