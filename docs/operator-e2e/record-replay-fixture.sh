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
  kiro)     cmd=(kiro-cli chat) ;;
  opencode) cmd=(opencode --standalone) ;;
  *) echo "usage: $0 claude|codex|kiro|opencode" >&2; exit 2 ;;
esac
[ -x "$REC" ] || { echo "record-replay-fixture: no recorder at $REC (build it: cargo build --release --example record_tail)" >&2; exit 1; }
command -v "${cmd[0]}" >/dev/null || { echo "record-replay-fixture: ${cmd[0]} is not on PATH" >&2; exit 1; }

out="$REPO/tests/fixtures/replay/$agent"
work="$(mktemp -d "${TMPDIR:-/tmp}/replay-$agent.XXXXXX")"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"
echo "record-replay-fixture: $agent, output $out, scratch directory $work"

record() { # record NAME [recorder options...]
  local name="$1"; shift
  "$REC" --out "$out" --name "$name" --cwd "$work" --quiet-ms 5000 --max-secs 240 "$@" -- "${cmd[@]}"
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
