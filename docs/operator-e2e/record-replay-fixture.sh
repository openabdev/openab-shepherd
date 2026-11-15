#!/usr/bin/env bash
# Record replay fixtures for one agent with examples/record_tail.rs (issue #76). Run by hand
# on a machine where that agent's CLI is installed and logged in. It uses TWO model prompts:
#   01-startup            no prompt: the CLI's first screen
#   02-turn               one prompt that needs no tool, so a full turn is recorded
#   03-permission-dialog  one prompt that makes the agent ask to run a shell command; the
#                         recording stops with the dialog open and nothing is approved
# The files go to tests/fixtures/replay/<agent>/ as NN-<scenario>.raw, .screen.txt and .json.
# Read them before you commit: the screen can show your directory names, account names or
# other things you do not want in a public repository. record_tail replaces your home
# directory and refuses to write anything that looks like a credential.
#
#   bash record-replay-fixture.sh claude|codex|kiro|opencode
#   RECORD_PASS_ENV="KIRO_API_KEY" bash record-replay-fixture.sh kiro
#
# RECORD_PASS_ENV is a space-separated list of variable NAMES (default empty) that the
# recorder copies into the pane, for an agent that logs in by key. The values are never
# printed or stored, and the recorder refuses to write a file that contains one.
# For kiro the script writes a workspace agent `probe` that trusts no tool.
# For opencode the script writes a throwaway opencode.jsonc with ask rules for shell and
# edit into the scratch directory, so the third scenario ends on a permission dialog.
#
# Build the recorder on the build host first (the laptop never runs cargo):
#   cargo build --release --example record_tail
# and point REC at it if it is not at target/release/examples/record_tail.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
REC="${REC:-$REPO/target/release/examples/record_tail}"
agent="${1:-}"

case "$agent" in
  claude)   cmd=(claude) ;;
  codex)    cmd=(codex) ;;
  kiro)     cmd=(kiro-cli chat --agent probe --trust-workspace) ;;
  opencode) cmd=(opencode --standalone) ;;
  *) echo "usage: $0 claude|codex|kiro|opencode" >&2; exit 2 ;;
esac
[ -x "$REC" ] || { echo "record-replay-fixture: no recorder at $REC (build it: cargo build --release --example record_tail)" >&2; exit 1; }
command -v "${cmd[0]}" >/dev/null || { echo "record-replay-fixture: ${cmd[0]} is not on PATH" >&2; exit 1; }

out="$REPO/tests/fixtures/replay/$agent"
# Not $TMPDIR: on macOS it is /private/var/folders/<per-user hash>/T, and the agents print the
# working directory on screen, so the hash would land in the recording.
work="$(mktemp -d "/tmp/replay-$agent.XXXXXX")"
trap 'rm -rf "$work"' EXIT
if [ "$agent" = kiro ]; then
  # Never start the chat (it would open the auth portal) unless the pane's environment is logged in.
  env -i HOME="$HOME" PATH="$PATH" USER="${USER:-}" LANG="${LANG:-en_US.UTF-8}" TERM=xterm-256color \
    ${KIRO_API_KEY:+KIRO_API_KEY="$KIRO_API_KEY"} kiro-cli whoami >/dev/null 2>&1 \
    && echo "record-replay-fixture: kiro whoami PASS" \
    || { echo "record-replay-fixture: kiro whoami FAIL; not recording" >&2; exit 1; }
fi
mkdir -p "$out"
if [ "$agent" = kiro ]; then
  # A workspace agent that trusts no tool, so the shell prompt ends on an approval dialog.
  mkdir -p "$work/.kiro/agents"
  cat > "$work/.kiro/agents/probe.json" <<'JSON'
{
  "name": "probe",
  "description": "Recording agent that trusts no tool",
  "tools": ["@builtin"],
  "allowedTools": []
}
JSON
fi
if [ "$agent" = opencode ]; then
  cat > "$work/opencode.jsonc" <<'JSON'
{
  "$schema": "https://opencode.ai/config.json",
  "permissions": [
    { "action": "shell", "resource": "*", "effect": "ask" },
    { "action": "edit", "resource": "*", "effect": "ask" }
  ]
}
JSON
fi
pass=()
for v in ${RECORD_PASS_ENV:-}; do pass+=(--pass-env "$v"); done
echo "record-replay-fixture: $agent, output $out, scratch directory $work"

record() { # record NAME [recorder options...]
  local name="$1"; shift
  "$REC" --out "$out" --name "$name" --cwd "$work" --quiet-ms 5000 --max-secs 240 ${pass[@]+"${pass[@]}"} "$@" -- "${cmd[@]}"
  local status=$?
  case "$status" in
    0) ;;
    3) echo "record-replay-fixture: the recorder found a possible secret in $name; nothing written for it" >&2; exit 3 ;;
    *) echo "record-replay-fixture: $name failed (status $status)" >&2; exit 1 ;;
  esac
}

record 01-startup
record 02-turn --send "Reply with only the word OK. Do not use any tools."
record 03-permission-dialog --send "Use the shell tool to run exactly this command in the current directory and nothing else: touch replay_marker.txt"

echo
echo "Recorded. Next, by hand:"
echo "  1. read every .screen.txt and skim the .raw files (strings $out/*.raw | less)"
echo "  2. check .json: gaps must be 0, and 03 must end blocked on the dialog"
echo "  3. git add $out and commit"
ls -l "$out"
