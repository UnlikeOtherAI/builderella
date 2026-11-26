#!/usr/bin/env bash
# Run a command against the current commit on another machine over SSH.
#
# The commit is pushed to origin first, then fetched into a dedicated clone
# on the remote host, so the remote always builds an exact, named revision.
#
#   BELLA_REMOTE=user@host \
#   BELLA_REMOTE_DIR='C:\work\builderella-check' \
#   BELLA_REMOTE_SHELL=powershell \
#   scripts/dev/remote-check.sh cargo test -p bella-store
#
# BELLA_REMOTE        SSH target (required)
# BELLA_REMOTE_DIR    Clone directory on the remote, used only by this script (required)
# BELLA_REMOTE_SHELL  `sh` (default) or `powershell`
# BELLA_REMOTE_PATH   Extra PATH entry on the remote, e.g. the cargo bin directory
#
# Arguments are joined with spaces and run by the remote shell.
set -euo pipefail

: "${BELLA_REMOTE:?set BELLA_REMOTE to an ssh target}"
: "${BELLA_REMOTE_DIR:?set BELLA_REMOTE_DIR to a clone directory on the remote}"
shell="${BELLA_REMOTE_SHELL:-sh}"
extra_path="${BELLA_REMOTE_PATH:-}"

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "remote-check: commit your changes first; only committed work is sent" >&2
  exit 2
fi

branch="$(git branch --show-current)"
sha="$(git rev-parse HEAD)"
origin="$(git remote get-url origin)"
git push --quiet origin "HEAD:refs/heads/${branch}"

cmd="$*"
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10)

if [ "$shell" = "powershell" ]; then
  {
    printf '%s\n' "\$ErrorActionPreference = 'Stop'"
    if [ -n "$extra_path" ]; then
      printf '%s\n' "\$env:PATH = '${extra_path};' + \$env:PATH"
    fi
    printf '%s\n' "if (-not (Test-Path '${BELLA_REMOTE_DIR}\\.git')) { git clone --quiet --no-checkout '${origin}' '${BELLA_REMOTE_DIR}' }"
    printf '%s\n' "Set-Location '${BELLA_REMOTE_DIR}'"
    printf '%s\n' "git fetch --quiet origin ${sha}"
    printf '%s\n' "git checkout --quiet --detach ${sha}"
    printf '%s\n' "\$ErrorActionPreference = 'Continue'"
    printf '%s\n' "${cmd}"
    printf '%s\n' "exit \$LASTEXITCODE"
  } | ssh "${ssh_opts[@]}" "$BELLA_REMOTE" 'powershell -NoProfile -NonInteractive -Command -'
else
  {
    printf '%s\n' "set -e"
    if [ -n "$extra_path" ]; then
      printf '%s\n' "export PATH='${extra_path}':\"\$PATH\""
    fi
    printf '%s\n' "[ -d '${BELLA_REMOTE_DIR}/.git' ] || git clone --quiet --no-checkout '${origin}' '${BELLA_REMOTE_DIR}'"
    printf '%s\n' "cd '${BELLA_REMOTE_DIR}'"
    printf '%s\n' "git fetch --quiet origin ${sha}"
    printf '%s\n' "git checkout --quiet --detach ${sha}"
    printf '%s\n' "${cmd}"
  } | ssh "${ssh_opts[@]}" "$BELLA_REMOTE" 'sh -s'
fi
