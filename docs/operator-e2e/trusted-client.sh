#!/usr/bin/env bash
# Trusted clients (issue #113) on a real server: call-level `extra_args` behind the scope
# `start:args`, the `agent` field on argv starts, the Kiro trust-all guard, and the audit
# fields. The checks are listed in trusted-client.md. Takes about 2 minutes.
# Calls no model: the panes are `sh -c` scripts that print OSC 7501 status reports, a Kiro
# trust-all warning or a recorded Kiro permission dialog, and write what they were given
# (their arguments, what is typed) to files. Every verdict comes from MCP fields, those
# files and the audit file. The real kiro-cli steps (persistence of the warning, a recorded
# screen) are by hand: see trusted-client.md.
# RUN IT YOURSELF: agents never start the Shepherd server.
#
#   Step  Where
#   1     t_setup, t_start_holder: profiles file (allow_arbitrary_command true), clients file, audit file
#   2     extra_args: appended after the profile's arguments; no scope, no list entry, no call
#   3     extra_args refusals: control character, empty, too long, too many, with argv, without profile,
#         the OpenCode auto-approve flag
#   4     the Kiro trust-all guard: pane_needs_trust_acceptance, nothing typed, pane left open;
#         after the warning is gone a send works
#   5     agent on an argv start: answer_dialog allow_once works with it and is refused without
#   6     the audit file: launch, agent, extra_args_n, flags, argv0, and no argument value
#
# Nothing in this script is Linux-only or macOS-only (it does not use RLIMIT_FSIZE, which
# macOS does not enforce, nor `stat -f`); r_skip exists for a step that ever must be. The
# summary line counts passed, failed and skipped checks; RESULT is decided by the failures.
# Every read of a file or a field that a pane or the server writes after the call returned
# waits (t_until, bounded); every "nothing happened" check watches a window (t_stays).
#
# Settings (environment): SHEPHERD_BIN, PORT (default 8768), R_HOME, R_WORK (both must contain
# shepherd-e2e-restart), KEEP=1 (leave everything behind).
#
# `T_SOURCE_ONLY=1 source trusted-client.sh` defines the functions and runs nothing; the
# repository's tests use it to check the files t_setup writes and to run the helpers
# against canned replies.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
R_HOME="${R_HOME:-$HOME/.config/shepherd-e2e-restart-trusted}"
R_WORK="${R_WORK:-/tmp/shepherd-e2e-restart-trusted}"
PORT="${PORT:-8768}"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

T_AUDIT_DIR="$R_HOME/audit"
T_AUDIT="$T_AUDIT_DIR/audit.jsonl"
T_CLIENTS="$R_HOME/clients.json"
T_PANES="$R_WORK/panes"
T_WARNING="trust all tools mode"
T_WAIT=20                              # seconds a pane or the server gets to write something
T_WINDOW=3                             # seconds a "nothing happened" check watches
export SHEPHERD_TOKEN_FILE="$R_HOME/ops.token"   # `ops` starts panes; r_mcp_up uses it

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

# t_write_profiles HOME PANES : the profiles file. Everything it needs is an argument.
t_write_profiles() {
  python3 - "$1" "$2" <<'PY'
import json, os, sys

home, panes = sys.argv[1], sys.argv[2]
WORKING = r"\033]7501;state=working\007"
IDLE = r"\033]7501;state=idle\007"
DONE = r"\033]7501;state=done\007"
SLEEP = "exec sleep 31337"            # a unique duration, so a leftover pane is easy to find
# `sandbox`: records the arguments after its own name, then appends what is typed to typed.txt.
# The profile's argv ends with "sandbox" ($0), so the call's extra_args are $1, $2, ...
sandbox = (f"printf '{WORKING}'; printf '{DONE}'; "
           "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > args.txt; "
           "echo ready > ready.txt; cat >> typed.txt")
# `guard`: Kiro's trust-all warning on the screen with `No, exit` as the default, idle status,
# and nothing read until the file `accept` exists (a person accepting it). Then the screen
# is cleared and what is typed goes to typed.txt. A line typed too early lands in typed.txt
# too, so a test sees it.
warning = ("Kiro is running in trust all tools mode\\n\\n"
           "  > No, exit\\n    Yes, and do not ask again\\n")
guard = (f"printf '{IDLE}'; printf '{warning}'; echo ready > ready.txt; "
         "while [ ! -e accept ]; do sleep 1; done; printf '\\033[2J\\033[H'; "
         "echo accepted > accepted.txt; cat >> typed.txt")
other = f"printf '{WORKING}'; printf '{DONE}'; {SLEEP}"


def profile(name, agent, script):
    return {"name": name, "agent": agent, "argv": ["/bin/sh", "-c", script, name],
            "cwds": [panes]}


profiles = [profile("sandbox", "kiro", sandbox),
            profile("guard", "kiro", guard),
            profile("other", "kiro", other),
            profile("oc", "opencode", other)]
path = os.path.join(home, "profiles.json")
with open(path, "w") as f:
    json.dump({"allow_arbitrary_command": True, "profiles": profiles}, f, indent=2)
    f.write("\n")
os.chmod(path, 0o600)
PY
}

# t_write_clients OUT OPS_HASH ORCH_HASH TRUSTED_HASH : the clients file; the hashes are arguments.
t_write_clients() {
  python3 - "$1" "$2" "$3" "$4" <<'PY'
import json, os, sys

out, ops, orch, trusted = sys.argv[1:]
doc = {"version": 1, "clients": [
    # Starts profile panes and looks after them. No start:args, no start:arbitrary.
    {"id": "ops", "token_sha256": ops,
     "scopes": ["observe", "read", "control", "start", "remove"],
     "profiles": ["sandbox", "guard", "other", "oc"]},
    # A trusted orchestrator that may add flags to two profiles. `other` is not on its list.
    {"id": "orch", "token_sha256": orch,
     "scopes": ["observe", "read", "control", "start", "start:args", "remove"],
     "profiles": ["sandbox", "guard", "oc"]},
    # A trusted client that also runs its own program and answers dialogs.
    {"id": "trusted", "token_sha256": trusted,
     "scopes": ["observe", "read", "control", "start", "start:arbitrary", "start:args",
                "dialog", "remove"],
     "profiles": ["sandbox"]},
]}
with open(out, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
os.chmod(out, 0o600)
PY
}

# t_write_dialog FILE : a recorded Kiro permission dialog, the text src/dialog.rs parses in its tests.
t_write_dialog() {
  python3 - "$1" <<'PY'
import sys

bar = "\u2500" * 72
text = (
    "  Use the shell tool to run exactly this command in the current directory and nothing else: touch shepherd_marker.txt\n\n"
    "\u2193 Shell touch shepherd_marker.txt\n"
    "    \u2570 description=Create an empty marker file, cwd=/tmp/shepherd-e2e-restart, run_in_background=false\n\n"
    + bar + "\n"
    " Create an empty marker file requires approval\n"
    " shell \u2192 touch shepherd_marker.txt\n\n"
    " \u276f Allow once\n"
    "   Deny once\n"
    "   Create allow rule to approve future similar actions\n"
    "   Create deny rule to block future similar actions\n"
    + bar + "\n"
    " esc to close \u00b7 \u2191\u2193 to navigate \u00b7 \u21b5 to select \u00b7 Tab to edit\n"
)
with open(sys.argv[1], "w", encoding="utf-8") as f:
    f.write(text)
PY
}

# t_setup : tokens, clients file, audit directory, profiles, pane directories. Safe to run again.
t_setup() {
  [ -x "$BIN" ] || r_die "no executable at $BIN (build it on the build host and copy it, see restart-survival.md)"
  for tool in python3 openssl curl pgrep; do
    command -v "$tool" >/dev/null || r_die "$tool is not installed"
  done
  case "$R_WORK" in */shepherd-e2e-restart*) ;; *) r_die "refusing to use R_WORK=$R_WORK (the path must contain shepherd-e2e-restart)" ;; esac
  case "$R_HOME" in */shepherd-e2e-restart*) ;; *) r_die "refusing to use R_HOME=$R_HOME (the path must contain shepherd-e2e-restart)" ;; esac
  if git -C "$R_HOME" rev-parse --show-toplevel >/dev/null 2>&1; then
    r_die "$R_HOME is inside a git repository; the tokens and the clients file must be outside every repository"
  fi
  (
    umask 077
    mkdir -p "$R_HOME" "$R_WORK" "$T_AUDIT_DIR"
    chmod 700 "$R_HOME" "$R_WORK" "$T_AUDIT_DIR"
    rm -f "$T_AUDIT"
    rm -rf "${R_WORK:?}/server" "${T_PANES:?}"
    mkdir -p "$R_WORK/server" "$T_PANES"
    for id in ops orch trusted; do
      file="$R_HOME/$id.token"
      [ -s "$file" ] || printf 'osh_%s_%s' "$id" "$(openssl rand -hex 32)" > "$file"
      chmod 600 "$file"
    done
    hash_of() { openssl dgst -sha256 < "$R_HOME/$1.token" | awk '{print $NF}'; }
    t_write_clients "$T_CLIENTS" "$(hash_of ops)" "$(hash_of orch)" "$(hash_of trusted)"
  ) || r_die "setup failed"
  t_write_profiles "$R_HOME" "$T_PANES" || r_die "writing the profiles failed"
  : > "$R_VERSION_FILE"
  r_say "setup done: tokens, clients and profiles in $R_HOME, panes in $R_WORK"
}

# t_dir NAME : make the working directory of pane NAME and print it. `dialog.txt` is in every one.
t_dir() {
  mkdir -p "$T_PANES/$1" && t_write_dialog "$T_PANES/$1/dialog.txt" && echo "$T_PANES/$1"
}

# t_start_holder : `openab-shepherd hold` with the clients file and the audit file, in the
# background; waits for MCP.
t_start_holder() {
  if r_port_in_use "$PORT"; then r_die "port $PORT is in use; set PORT=<another port>"; fi
  : > "$R_LOG"
  (
    cd "$R_WORK/server" || exit 1
    exec env -u OPENAB_SHEPHERD_BACKEND -u OPENAB_SHEPHERD_TOKEN -u OPENAB_SHEPHERD_TOKEN_FILE \
      OPENAB_SHEPHERD_CLIENTS="$T_CLIENTS" \
      OPENAB_SHEPHERD_AUDIT_FILE="$T_AUDIT" \
      OPENAB_SHEPHERD_PROFILES="$R_HOME/profiles.json" \
      OPENAB_SHEPHERD_TEST_HOLDER_VERSION="@$R_VERSION_FILE" \
      "$BIN" hold -- --host 127.0.0.1 --port "$PORT"
  ) >> "$R_LOG" 2>&1 < /dev/null &
  echo $! > "$R_PIDFILE"
  r_wait_mcp 30 || { tail -20 "$R_LOG" >&2; r_die "the server did not come up within 30 s"; }
  r_say "holder pid $(r_holder_pid), front pid $(r_front_pid), port $PORT, log $R_LOG"
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# as CLIENT CMD... : run an MCP helper with that client's token.
as() { local id="$1"; shift; SHEPHERD_TOKEN_FILE="$R_HOME/$id.token" "$@"; }

# t_py EXPRESSION : evaluate a Python expression on the JSON from stdin (`d`); "null" for None,
# nothing at all when the input is not JSON or the expression fails.
t_py() {
  python3 -c 'import json, sys
d = json.load(sys.stdin)
v = eval(sys.argv[1])
print("null" if v is None else (v if isinstance(v, str) else json.dumps(v)))' "$1" 2>/dev/null
}

# t_until SECONDS CMD... : run CMD until it succeeds; one last try at the deadline. A bounded wait.
t_until() {
  local end=$((SECONDS + $1))
  shift
  while :; do
    "$@" && return 0
    [ "$SECONDS" -ge "$end" ] && return 1
    sleep 0.5
  done
}

# t_stays SECONDS CMD... : CMD must succeed now and at every half second of the window.
t_stays() {
  local end=$((SECONDS + $1))
  shift
  while :; do
    "$@" || return 1
    [ "$SECONDS" -ge "$end" ] && return 0
    sleep 0.5
  done
}

# t_out_is EXPECTED CMD... : CMD's output (trailing newlines dropped) is EXPECTED.
t_out_is() {
  local want="$1"
  shift
  [ "$("$@" 2>/dev/null)" = "$want" ]
}

# t_file_is FILE EXPECTED : the file exists and holds exactly EXPECTED (trailing newlines dropped).
t_file_is() { [ -f "$1" ] && [ "$(cat "$1")" = "$2" ]; }

# t_absent FILE
t_absent() { [ ! -e "$1" ]; }

# t_json_is EXPECTED EXPRESSION CMD... : CMD's JSON output, through t_py, is EXPECTED.
t_json_is() {
  local want="$1" expr="$2"
  shift 2
  [ "$("$@" 2>/dev/null | t_py "$expr")" = "$want" ]
}

# t_screen_has NAME TEXT / t_screen_lacks NAME TEXT : the pane's visible screen.
t_screen_has() { read_screen "$1" 2>/dev/null | grep -q -- "$2"; }
t_screen_lacks() { ! t_screen_has "$1" "$2"; }

# t_no_pane NAME : list_panes shows no pane of that name (a refused start leaves none).
t_no_pane() { ! list_panes true 2>/dev/null | grep -q "\"name\": \"$1\""; }

# t_has_pane NAME : list_panes shows a pane of that name (the control for t_no_pane).
t_has_pane() { list_panes true 2>/dev/null | grep -q "\"name\": \"$1\""; }

# t_list_json COUNT WIDTH : a JSON list of COUNT strings of WIDTH x characters, for a long argument.
t_list_json() {
  python3 -c 'import json, sys
count, width = int(sys.argv[1]), int(sys.argv[2])
print(json.dumps(["x" * width] * count))' "$1" "$2"
}

# t_audit_check PYTHON_BODY : run Python over the audit records once (`records`, a list of dicts;
# `raw`, the file's text). Success when the expression is true.
t_audit_check() {
  python3 - "$T_AUDIT" "$1" <<'PY'
import json, sys

path, body = sys.argv[1:]
try:
    raw = open(path).read()
except OSError:
    sys.exit(1)
records = []
for line in raw.splitlines():
    try:
        records.append(json.loads(line))
    except ValueError:
        pass                              # a partial line
sys.exit(0 if eval(body, {"records": records, "raw": raw}) else 1)
PY
}

# t_audit_wait PYTHON_BODY : wait (bounded) until the audit file satisfies the expression.
t_audit_wait() { t_until "$T_WAIT" t_audit_check "$1"; }

# t_start CLIENT NAME PROFILE [key:=json ...] : start_pane in the pane's own directory.
t_start() {
  local client="$1" name="$2" profile="$3" dir
  shift 3
  dir="$(t_dir "$name")" || return 1
  as "$client" mcp_tool start_pane "profile=$profile" "cwd=$dir" "name=$name" "$@"
}

# t_start_argv CLIENT NAME ARGV_JSON [key=value ...] : an argv start in the pane's own directory.
t_start_argv() {
  local client="$1" name="$2" argv="$3" dir
  shift 3
  dir="$(t_dir "$name")" || return 1
  as "$client" mcp_tool start_pane "argv:=$argv" "cwd=$dir" "name=$name" "$@"
}

# t_error CMD... -> the error code of a failing call; t_field EXPR CMD... -> a field of its output.
t_error() { "$@" 2>/dev/null | python3 -c 'import json, sys
try:
    print(json.load(sys.stdin).get("error", ""))
except Exception:
    print("")'; }

# ---------------------------------------------------------------------------
# The scenario
# ---------------------------------------------------------------------------

t_main() {
  t_setup
  trap r_cleanup EXIT
  t_start_holder
  local out

  # --- Step 2: extra_args behind start:args --------------------------------------------------
  out="$(t_start orch x1 sandbox 'extra_args:=["-a", "--model", "claude-sonnet-4"]' 2>/dev/null)"
  r_check "orch (start:args): extra_args are appended after the profile's argv" \
    test "$(echo "$out" | t_py 'd["argv"][3:]')" = '["sandbox", "-a", "--model", "claude-sonnet-4"]'
  r_check "...and the program and its first arguments are the profile's" \
    test "$(echo "$out" | t_py 'd["argv"][:2]')" = '["/bin/sh", "-c"]'
  r_check "the pane received the flags as its arguments" \
    t_until "$T_WAIT" t_file_is "$T_PANES/x1/args.txt" "$(printf -- '-a\n--model\nclaude-sonnet-4')"

  r_check "ops (no start:args): extra_args are forbidden" \
    test "$(t_error t_start ops rej sandbox 'extra_args:=["-a"]')" = forbidden
  r_check "...and the error names the scope start:args" \
    test "$(t_start ops rej sandbox 'extra_args:=["-a"]' 2>/dev/null | t_py 'd["scope"]')" = start:args
  r_check "orch: a profile that is not on its list is forbidden even with start:args" \
    test "$(t_error t_start orch rej other 'extra_args:=["-a"]')" = forbidden
  r_check "start:arbitrary does not stand in for start:args" \
    test "$(t_error t_start trusted rej sandbox 'extra_args:=["-a"]')" = forbidden
  r_check "...and the error names the scope start:args, not the profile list" \
    test "$(t_start trusted rej sandbox 'extra_args:=["-a"]' 2>/dev/null | t_py 'd["scope"]')" = start:args
  r_check "extra_args null is the same as none: ops may start" \
    r_quiet t_start ops x2 sandbox 'extra_args:=null'
  r_check "extra_args [] is the same as none: ops may start" \
    r_quiet t_start ops x3 sandbox 'extra_args:=[]'
  r_check "...null added no argument" \
    t_until "$T_WAIT" t_file_is "$T_PANES/x2/args.txt" ""
  r_check "...and the empty list added none either" \
    t_until "$T_WAIT" t_file_is "$T_PANES/x3/args.txt" ""

  # --- Step 3: refusals ---------------------------------------------------------------------
  r_check "a control character in an element is invalid_input" \
    test "$(t_error t_start orch rej sandbox 'extra_args:=["bad\u0001x"]')" = invalid_input
  r_check "...and the error names the field extra_args" \
    test "$(t_start orch rej sandbox 'extra_args:=["bad\u0001x"]' 2>/dev/null | t_py 'd["field"]')" = extra_args
  r_check "an empty element is invalid_input" \
    test "$(t_error t_start orch rej sandbox 'extra_args:=[""]')" = invalid_input
  r_check "an element over 4096 bytes is invalid_input" \
    test "$(t_error t_start orch rej sandbox "extra_args:=$(t_list_json 1 5000)")" = invalid_input
  r_check "more than 256 arguments with the profile's are invalid_input" \
    test "$(t_error t_start orch rej sandbox "extra_args:=$(t_list_json 256 1)")" = invalid_input
  r_check "extra_args without a profile are invalid_input" \
    test "$(t_error as trusted mcp_tool start_pane 'extra_args:=["-a"]' name=rej)" = invalid_input
  r_check "extra_args with argv are invalid_input" \
    test "$(t_error t_start_argv trusted rej '["/bin/sh", "-c", "true"]' 'extra_args:=["-a"]')" = invalid_input
  r_check "the OpenCode auto-approve flag in extra_args is invalid_input" \
    test "$(t_error t_start orch rej oc 'extra_args:=["--auto"]')" = invalid_input
  r_check "...and the error names the field extra_args" \
    test "$(t_start orch rej oc 'extra_args:=["--auto"]' 2>/dev/null | t_py 'd["field"]')" = extra_args
  r_check "none of the refused starts left a pane" t_no_pane rej

  # --- Step 4: the Kiro trust-all guard -----------------------------------------------------
  r_check "orch starts the guard pane (a Kiro pane showing the trust-all warning)" \
    r_quiet t_start orch g1 guard 'extra_args:=["-a"]'
  r_check "the guard pane has printed its screen" t_until "$T_WAIT" t_file_is "$T_PANES/g1/ready.txt" ready
  r_check "the screen shows the warning" t_until "$T_WAIT" t_screen_has g1 "$T_WARNING"
  r_check "send_input is pane_needs_trust_acceptance" \
    test "$(t_error as ops send_input g1 hello)" = pane_needs_trust_acceptance
  r_check "...and the error names the pane and the next step" \
    test "$(as ops send_input g1 hello 2>/dev/null | t_py 'd["terminal"]')" = g1
  r_check "nothing was typed into the pane (it reads nothing before accept)" \
    t_stays "$T_WINDOW" t_absent "$T_PANES/g1/typed.txt"
  r_check "the pane is still open" t_json_is true 'd["live"]' as ops get_status g1
  r_check "answer_dialog is refused too (the pane is not blocked), no key sent" \
    test "$(t_error as trusted answer_dialog g1 allow_once)" = refused
  touch "$T_PANES/g1/accept"                    # a person accepts the warning
  r_check "the warning is accepted: the pane printed accepted.txt" \
    t_until "$T_WAIT" t_file_is "$T_PANES/g1/accepted.txt" accepted
  r_check "the screen no longer shows the warning" t_until "$T_WAIT" t_screen_lacks g1 "$T_WARNING"
  r_check "send_input works once the warning is gone" r_quiet as ops send_input g1 hello
  r_check "the pane got exactly that line" t_until "$T_WAIT" t_file_is "$T_PANES/g1/typed.txt" hello

  # --- Step 5: agent on an argv start -------------------------------------------------------
  local dialog_script
  dialog_script="printf '\\033]7501;state=working\\007'; printf '\\033]7501;state=blocked:kind=permission\\007'; cat dialog.txt; IFS= read -r l; echo got > received.txt; exec sleep 31337"
  local argv
  argv="$(python3 -c 'import json, sys; print(json.dumps(["/bin/sh", "-c", sys.argv[1]]))' "$dialog_script")"
  r_check "ops (no start:arbitrary): an argv start is forbidden" \
    test "$(t_error t_start_argv ops rej "$argv")" = forbidden
  r_check "trusted: an argv start with agent kiro is accepted" r_quiet t_start_argv trusted a-yes "$argv" agent=kiro
  r_check "trusted: the same argv without agent is accepted" r_quiet t_start_argv trusted a-no "$argv"
  r_check "a-yes is blocked on the dialog" r_wait_field a-yes status.state blocked "$T_WAIT"
  r_check "a-no is blocked on the dialog" r_wait_field a-no status.state blocked "$T_WAIT"
  r_check "a-yes: answer_dialog allow_once works" r_quiet as trusted answer_dialog a-yes allow_once
  r_check "a-yes: the pane received the answer" t_until "$T_WAIT" t_file_is "$T_PANES/a-yes/received.txt" got
  r_check "a-no: answer_dialog is refused (no agent, no dialect)" \
    test "$(t_error as trusted answer_dialog a-no allow_once)" = refused
  r_check "a-no: no key was sent" t_stays "$T_WINDOW" t_absent "$T_PANES/a-no/received.txt"
  r_check "agent with a profile is invalid_input" \
    test "$(t_error t_start trusted rej sandbox agent=kiro)" = invalid_input
  r_check "an unknown agent is invalid_input" \
    test "$(t_error t_start_argv trusted rej "$argv" agent=emacs)" = invalid_input
  r_check "no refused start left a pane" t_no_pane rej
  r_check "...and the check can see a pane that exists (control: x1 is listed)" \
    t_has_pane x1

  # --- Step 6: the audit file ---------------------------------------------------------------
  # A canary in the values: a flag value, a short flag with its value attached, a long flag
  # with a value after `=`. None may reach the file.
  r_check "orch: a start with canary values is accepted" \
    r_quiet t_start orch c1 sandbox 'extra_args:=["--api-key", "CANARYVALUE9", "-pCANARYSHORT", "--token=CANARYEQ"]'
  r_check "audit: the orch start is launch profile+args with 3 extra_args and the flag names" \
    t_audit_wait 'any(r["client"] == "orch" and r["tool"] == "start_pane" and r["phase"] == "call" and r.get("launch") == "profile+args" and r.get("extra_args_n") == 3 and r.get("flags") == ["-a", "--model"] for r in records)'
  r_check "audit: the canary start records only the flag names" \
    t_audit_wait 'any(r["client"] == "orch" and r["tool"] == "start_pane" and r["phase"] == "call" and r.get("extra_args_n") == 4 and r.get("flags") == ["--api-key", "--token"] for r in records)'
  r_check "audit: the argv start records launch argv, argv0 sh and agent kiro" \
    t_audit_wait 'any(r["client"] == "trusted" and r["tool"] == "start_pane" and r["phase"] == "call" and r.get("launch") == "argv" and r.get("argv0") == "sh" and r.get("agent") == "kiro" for r in records)'
  r_check "audit: the argv start without agent records no agent" \
    t_audit_wait 'any(r["client"] == "trusted" and r.get("launch") == "argv" and r.get("pane") == "a-no" and "agent" not in r for r in records)'
  r_check "audit: the denied extra_args start is a denied line under ops" \
    t_audit_wait 'any(r["client"] == "ops" and r["tool"] == "start_pane" and r["phase"] == "denied" for r in records)'
  r_check "audit: a plain profile start has no launch field" \
    t_audit_wait 'any(r["client"] == "ops" and r["tool"] == "start_pane" and r["phase"] == "call" and r.get("pane") == "x2" and "launch" not in r for r in records)'
  # Absence checks hold over a window (the positive checks above already waited for the lines).
  r_check "audit: no argument value is in the file (canary, model name, script text)" \
    t_stays "$T_WINDOW" t_audit_check '"CANARY" not in raw and "claude-sonnet-4" not in raw and "dialog.txt" not in raw and "printf" not in raw'
  r_check "audit: the typed text is not in the file" \
    t_stays "$T_WINDOW" t_audit_check '"hello" not in raw'

  r_finish
}

if [ "${T_SOURCE_ONLY:-}" != 1 ]; then t_main; fi
