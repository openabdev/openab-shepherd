#!/usr/bin/env bash
# Agent-created routines (issue #98) on a real server, the ten checks of routine-dynamic.md.
# Takes about 12 minutes (a 5 minute downtime of the front is in it; DOWN_SECS=<n> shortens it,
# 120 or more still shows that nothing is made up). Calls no model: the panes are `sh -c`
# scripts that print OSC 7501 status reports and append what is typed to typed.txt, and the
# MCP calls an agent would make are made here with the agent's own token (client `agent-a`,
# scopes `routine` and `observe`). A second client `ops` (no `routine` scope) starts the panes.
# RUN IT YOURSELF: agents never start the Shepherd server.
#
#   Step  Where
#   1     d_setup, d_start_holder: profile with dynamic_routines, clients file, audit file, hold
#   2     panes start, SHEPHERD_PANE is the pane's name
#   3     routine_create, the refusals, routine_list
#   4     a tick types when the pane is done; a working pane is skipped and not made up
#   5     SIGUSR2: same grid, no nudge in the first seconds
#   6     the front is down for DOWN_SECS: no catch-up burst afterwards
#   7     the audit file: create call+outcome under agent-a, nudges under system:routine, no text
#   8     dynamic_routines removed + SIGUSR2: stopped / profile_changed, nothing types, delete works
#   9     the audit file cannot be written: routine_create is audit_unavailable, nothing created
#   10    an expiring routine shows expired and types nothing more (checked after step 6)
#
# Step 9 needs a write that fails on demand. The holder starts with a file-size limit
# (RLIMIT_FSIZE, SIGXFSZ ignored); the script then pads the audit file to the limit, so the
# next append fails with EFBIG. Changing the audit directory's mode would not do it: the file
# is already open.
#
# Settings (environment): SHEPHERD_BIN, PORT (default 8767), R_HOME, R_WORK (both must contain
# shepherd-e2e-restart), DOWN_SECS (default 300), KEEP=1 (leave everything behind).
#
# `D_SOURCE_ONLY=1 source routine-dynamic.sh` defines the functions and runs nothing; the
# repository's tests use it to check the files d_setup writes.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
R_HOME="${R_HOME:-$HOME/.config/shepherd-e2e-restart-dynamic}"
R_WORK="${R_WORK:-/tmp/shepherd-e2e-restart-dynamic}"
PORT="${PORT:-8767}"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

D_AUDIT_DIR="$R_HOME/audit"
D_AUDIT="$D_AUDIT_DIR/audit.jsonl"
D_CLIENTS="$R_HOME/clients.json"
D_FSIZE=8388608                       # the holder's file-size limit, in bytes (step 9)
D_NUDGE_TEXT="check whether any todo work is left"
export SHEPHERD_TOKEN_FILE="$R_HOME/ops.token"   # `ops` starts panes; r_mcp_up uses it

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

# d_write_profiles on|off : the profiles file; `off` leaves dynamic_routines out of `dyn`.
d_write_profiles() {
  R_WORK="$R_WORK" R_HOME="$R_HOME" D_DYNAMIC="$1" python3 - <<'PY'
import json, os

work, home = os.environ["R_WORK"], os.environ["R_HOME"]
WORKING = r"\033]7501;state=working\007"
DONE = r"\033]7501;state=done\007"
SLEEP = "exec sleep 31337"            # a unique duration, so a leftover pane is easy to find
# Every pane records its SHEPHERD_PANE. `busy` stays working until its `finish` file exists.
script = (f"printf '{WORKING}'; echo \"$SHEPHERD_PANE\" > pane-env.txt; "
          "if [ \"$SHEPHERD_PANE\" = busy ]; then while [ ! -e finish ]; do sleep 1; done; fi; "
          f"printf '{DONE}'; cat >> typed.txt")
dyn = {"name": "dyn", "agent": "kiro", "argv": ["/bin/sh", "-c", script],
       "cwds": [os.path.join(work, n) for n in ("dyn", "busy", "exp")]}
if os.environ["D_DYNAMIC"] == "on":
    dyn["dynamic_routines"] = {"min_every_secs": 60, "max_expires_secs": 3600}
plain = {"name": "plain", "agent": "kiro",
         "argv": ["/bin/sh", "-c", f"printf '{WORKING}'; printf '{DONE}'; {SLEEP}"],
         "cwds": [os.path.join(work, "plain")]}
path = os.path.join(home, "profiles.json")
with open(path, "w") as f:
    json.dump({"allow_arbitrary_command": False, "profiles": [dyn, plain]}, f, indent=2)
    f.write("\n")
os.chmod(path, 0o600)
PY
}

# d_setup : tokens, clients file, audit directory, profiles, working directories.
# Safe to run again. The tokens are made once per R_HOME.
d_setup() {
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
    mkdir -p "$R_HOME" "$R_WORK" "$D_AUDIT_DIR"
    chmod 700 "$R_HOME" "$R_WORK" "$D_AUDIT_DIR"
    rm -f "$D_AUDIT"
    for d in server dyn busy exp plain; do rm -rf "${R_WORK:?}/$d"; mkdir -p "$R_WORK/$d"; done
    printf -- '- [ ] one item that stays open\n' > "$R_WORK/dyn/todo.md"
    # A token is osh_<id>_<64 hex>; the clients file holds the SHA-256 of the whole token.
    for id in ops agent-a; do
      file="$R_HOME/$id.token"
      [ -s "$file" ] || printf 'osh_%s_%s' "$id" "$(openssl rand -hex 32)" > "$file"
      chmod 600 "$file"
    done
    ops_hash="$(openssl dgst -sha256 < "$R_HOME/ops.token" | awk '{print $NF}')"
    agent_hash="$(openssl dgst -sha256 < "$R_HOME/agent-a.token" | awk '{print $NF}')"
    OPS_HASH="$ops_hash" AGENT_HASH="$agent_hash" python3 - "$D_CLIENTS" <<'PY'
import json, os, sys
doc = {"version": 1, "clients": [
    # Starts and looks after the panes. No `routine` scope.
    {"id": "ops", "token_sha256": os.environ["OPS_HASH"],
     "scopes": ["observe", "read", "control", "start", "remove"], "profiles": ["dyn", "plain"]},
    # The agent: routine and observe, no control.
    {"id": "agent-a", "token_sha256": os.environ["AGENT_HASH"], "scopes": ["routine", "observe"]},
]}
with open(sys.argv[1], "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
os.chmod(sys.argv[1], 0o600)
PY
  ) || r_die "setup failed"
  d_write_profiles on || r_die "writing the profiles failed"
  : > "$R_VERSION_FILE"
  r_say "setup done: tokens, clients and profiles in $R_HOME, panes in $R_WORK"
}

# d_start_holder : `openab-shepherd hold` with the clients file, the audit file and a file-size
# limit (see the header), started in the background; waits for MCP.
d_start_holder() {
  if r_port_in_use "$PORT"; then r_die "port $PORT is in use; set PORT=<another port>"; fi
  : > "$R_LOG"
  (
    cd "$R_WORK/server" || exit 1
    exec env -u OPENAB_SHEPHERD_BACKEND -u OPENAB_SHEPHERD_TOKEN -u OPENAB_SHEPHERD_TOKEN_FILE \
      OPENAB_SHEPHERD_CLIENTS="$D_CLIENTS" \
      OPENAB_SHEPHERD_AUDIT_FILE="$D_AUDIT" \
      OPENAB_SHEPHERD_PROFILES="$R_HOME/profiles.json" \
      OPENAB_SHEPHERD_TEST_HOLDER_VERSION="@$R_VERSION_FILE" \
      python3 -c 'import os, resource, signal, sys
limit = int(sys.argv[1])
signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
resource.setrlimit(resource.RLIMIT_FSIZE, (limit, limit))
os.execv(sys.argv[2], sys.argv[2:])' "$D_FSIZE" "$BIN" hold -- --host 127.0.0.1 --port "$PORT"
  ) >> "$R_LOG" 2>&1 < /dev/null &
  echo $! > "$R_PIDFILE"
  r_wait_mcp 30 || { tail -20 "$R_LOG" >&2; r_die "the server did not come up within 30 s"; }
  r_say "holder pid $(r_holder_pid), front pid $(r_front_pid), port $PORT, log $R_LOG"
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# as_agent CMD... : run an MCP helper with agent-a's token.
as_agent() { SHEPHERD_TOKEN_FILE="$R_HOME/agent-a.token" "$@"; }

# rc_create TERMINAL EVERY_SECS TEXT [key:=json ...]
rc_create() {
  local terminal="$1" every="$2" text="$3"; shift 3
  mcp_tool routine_create "terminal=$terminal" "trigger:={\"type\": \"every\", \"every_secs\": $every}" "text=$text" "$@"
}
rc_list() { mcp_tool routine_list ${1:+"terminal=$1"}; }
rc_delete() { mcp_tool routine_delete "terminal=$1" "id=$2"; }

# d_py EXPRESSION : evaluate a Python expression on the JSON from stdin (`d`); "null" for None.
d_py() {
  python3 -c 'import json, sys
d = json.load(sys.stdin)
v = eval(sys.argv[1])
print("null" if v is None else (v if isinstance(v, str) else json.dumps(v)))' "$1" 2>/dev/null
}

# d_rfield TERMINAL ID FIELD : a field of one routine in routine_list ("missing" if absent).
d_rfield() {
  as_agent rc_list "$1" 2>/dev/null | python3 -c 'import json, sys
terminal, rid, field = sys.argv[1:]
for r in json.load(sys.stdin)["routines"]:
    if r["id"] == rid:
        v = r.get(field)
        print("null" if v is None else (v if isinstance(v, str) else json.dumps(v)))
        break
else:
    print("missing")' "$1" "$2" "$3" 2>/dev/null
}

# d_wait_rfield TERMINAL ID FIELD VALUE SECONDS
d_wait_rfield() {
  local end=$((SECONDS + $5))
  while [ "$SECONDS" -lt "$end" ]; do
    [ "$(d_rfield "$1" "$2" "$3")" = "$4" ] && return 0
    sleep 1
  done
  return 1
}

# d_wait_lines FILE MIN SECONDS : wait until FILE has at least MIN lines.
d_wait_lines() {
  local end=$((SECONDS + $3))
  while [ "$SECONDS" -lt "$end" ]; do
    [ "$(r_file_lines "$1")" -ge "$2" ] && return 0
    sleep 1
  done
  return 1
}

# d_wait_due_between TERMINAL ID LOW HIGH SECONDS : wait until next_due_in_secs is in LOW..HIGH.
d_wait_due_between() {
  local end=$((SECONDS + $5)) v
  while [ "$SECONDS" -lt "$end" ]; do
    v="$(d_rfield "$1" "$2" next_due_in_secs)"
    case "$v" in
      ''|*[!0-9]*) ;;
      *) if [ "$v" -ge "$3" ] && [ "$v" -le "$4" ]; then return 0; fi ;;
    esac
    sleep 1
  done
  return 1
}

# d_start NAME PROFILE : start a pane with the ops token, in the directory of its own name.
d_start() { start_pane "$2" "$R_WORK/$1" "$1" >/dev/null; }

d_typed() { r_file_lines "$R_WORK/$1/typed.txt"; }

# d_restart : SIGUSR2 on the holder, then wait for the new front.
d_restart() {
  local front
  front="$(r_front_pid)"
  kill -USR2 "$(r_holder_pid)"
  r_wait_new_front "$front" 30
}

# d_audit_check PYTHON_BODY : run Python over the audit records (`records`, a list of dicts).
d_audit_check() {
  python3 - "$D_AUDIT" "$D_NUDGE_TEXT" "$1" <<'PY'
import json, sys
path, text, body = sys.argv[1:]
records, raw = [], open(path).read()
for line in raw.splitlines():
    try:
        records.append(json.loads(line))
    except ValueError:
        pass                              # a partial or padded line
ok = eval(body, {"records": records, "raw": raw, "text": text})
sys.exit(0 if ok else 1)
PY
}

# ---------------------------------------------------------------------------
# The scenario
# ---------------------------------------------------------------------------

d_main() {
  d_setup
  trap r_cleanup EXIT
  d_start_holder
  local holder
  holder="$(r_holder_pid)"

  # --- Step 2: panes, and SHEPHERD_PANE ---------------------------------------------------
  d_start dyn dyn; d_start busy dyn; d_start exp dyn; d_start plain plain
  r_check "dyn is done" r_wait_field dyn status.state "done" 20
  r_check "exp is done" r_wait_field exp status.state "done" 20
  r_check "busy is working" r_wait_field busy status.state working 20
  r_check "dyn knows its own name in SHEPHERD_PANE" r_wait_file "$R_WORK/dyn/pane-env.txt" 10
  r_check "SHEPHERD_PANE is dyn" test "$(cat "$R_WORK/dyn/pane-env.txt" 2>/dev/null)" = dyn
  r_check "SHEPHERD_PANE is busy" test "$(cat "$R_WORK/busy/pane-env.txt" 2>/dev/null)" = busy

  # --- Step 3: create, the refusals, list ---------------------------------------------------
  local created
  created="$(as_agent rc_create dyn 60 "$D_NUDGE_TEXT" todo_file=todo.md expires_secs:=1800 2>/dev/null)"
  r_check "routine_create on dyn returns r1" test "$(echo "$created" | d_py 'd["routine"]["id"]')" = r1
  r_check "it names agent-a as the creator" test "$(echo "$created" | d_py 'd["routine"]["created_by"]')" = agent-a
  r_check "the result shows the ceiling" test "$(echo "$created" | d_py 'd["ceiling"]["min_every_secs"]')" = 60
  r_check "routine_list shows when it is due" test "$(d_rfield dyn r1 next_due_in_secs)" != null
  as_agent rc_create busy 60 "$D_NUDGE_TEXT" expires_secs:=1800 >/dev/null 2>&1
  as_agent rc_create exp 60 "$D_NUDGE_TEXT" expires_secs:=90 >/dev/null 2>&1
  r_check "busy has r1" test "$(d_rfield busy r1 status)" = active
  r_check "exp has r1" test "$(d_rfield exp r1 status)" = active

  r_check "every_secs below the floor is routine_invalid" \
    test "$(r_error as_agent rc_create dyn 30 "$D_NUDGE_TEXT")" = routine_invalid
  r_check "...and the error names the field" \
    test "$(as_agent rc_create dyn 30 "$D_NUDGE_TEXT" 2>/dev/null | d_py 'd["field"]')" = every_secs
  r_check "a bare confirmation is routine_invalid" \
    test "$(r_error as_agent rc_create dyn 60 yes)" = routine_invalid
  r_check "a todo_file outside the directory is routine_invalid" \
    test "$(r_error as_agent rc_create dyn 60 "$D_NUDGE_TEXT" todo_file=../escape.md)" = routine_invalid
  r_check "an idle trigger is routine_invalid" \
    test "$(r_error as_agent mcp_tool routine_create terminal=dyn 'trigger:={"type": "idle", "idle_secs": 60}' "text=$D_NUDGE_TEXT")" = routine_invalid
  r_check "a pane from a profile without dynamic_routines is routines_not_allowed" \
    test "$(r_error as_agent rc_create plain 60 "$D_NUDGE_TEXT")" = routines_not_allowed
  r_check "a client without the routine scope is forbidden" \
    test "$(r_error rc_list dyn)" = forbidden
  r_check "routine does not imply control: send_input is forbidden for agent-a" \
    test "$(r_error as_agent send_input dyn hello)" = forbidden
  r_check "the refused creates left one routine on dyn" \
    test "$(as_agent rc_list dyn 2>/dev/null | d_py 'len(d["routines"])')" = 1

  # --- Step 4: a tick types; a working pane is skipped and not made up ----------------------
  r_check "dyn is typed into at the first grid time" d_wait_lines "$R_WORK/dyn/typed.txt" 1 90
  r_check "the typed line is the routine's text" \
    test "$(head -1 "$R_WORK/dyn/typed.txt")" = "$D_NUDGE_TEXT"
  r_check "dyn: runs is 1 and last_result is sent" \
    test "$(d_rfield dyn r1 runs)/$(d_rfield dyn r1 last_result)" = "1/sent"
  r_check "busy: the tick found it working (skipped:working)" d_wait_rfield busy r1 last_result skipped:working 30
  r_check "busy: nothing was typed" test "$(d_typed busy)" = 0
  touch "$R_WORK/busy/finish"                       # the turn ends now; the missed tick is gone
  r_check "busy is done" r_wait_field busy status.state "done" 20
  sleep 15
  r_check "busy: the skipped tick is not made up after the turn ends" test "$(d_typed busy)" = 0
  r_check "busy is typed into at its next grid time, once" d_wait_lines "$R_WORK/busy/typed.txt" 1 90
  r_check "busy: exactly one line" test "$(d_typed busy)" = 1
  r_check "exp: typed once before it expires" test "$(d_typed exp)" = 1

  # --- Step 5: SIGUSR2 keeps the grid -------------------------------------------------------
  local due_before lines_before due_after
  r_check "dyn is 25 to 50 s from its next tick" d_wait_due_between dyn r1 25 50 120
  due_before="$(d_rfield dyn r1 next_due_at)"
  lines_before="$(d_typed dyn)"
  r_check "SIGUSR2: a new front serves MCP" d_restart
  due_after="$(d_rfield dyn r1 next_due_at)"
  r_check "the routine is listed again and is active" test "$(d_rfield dyn r1 status)" = active
  r_check "the grid is the same (the due times differ by whole intervals)" \
    test "$(( (due_after - due_before) % 60 ))" = 0 -a "$due_after" -ge "$due_before"
  sleep 8
  r_check "no nudge in the first seconds after the restart" test "$(d_typed dyn)" = "$lines_before"

  # --- Step 6: the front is down; nothing is made up ----------------------------------------
  local down="${DOWN_SECS:-300}" busy_before
  lines_before="$(d_typed dyn)"
  busy_before="$(d_typed busy)"
  echo 999 > "$R_VERSION_FILE"                      # a front of another protocol number exits 78
  kill -USR2 "$holder"
  sleep 5
  r_check "MCP is down" test "$(r_mcp_up && echo up || echo down)" = down
  r_say "the front stays down for $down s (DOWN_SECS)"
  sleep "$down"
  : > "$R_VERSION_FILE"
  r_check "a matching front attaches within 60 s" r_wait_mcp 60
  sleep 30
  r_check "dyn: at most one line since the front came back (no burst)" \
    test "$(( $(d_typed dyn) - lines_before ))" -le 1
  r_check "busy: at most one line since the front came back" \
    test "$(( $(d_typed busy) - busy_before ))" -le 1
  local created_at
  created_at="$(d_rfield dyn r1 created_at)"
  r_check "dyn's next due time is still on its grid" \
    test "$(( ($(d_rfield dyn r1 next_due_at) - created_at) % 60 ))" = 0
  r_check "dyn is still active" test "$(d_rfield dyn r1 status)" = active

  # --- Step 10 (here, after the downtime): the expiring routine ---------------------------
  r_check "exp: r1 is expired" d_wait_rfield exp r1 status expired 30
  r_check "exp: no next time" test "$(d_rfield exp r1 next_due_in_secs)" = null
  r_check "exp: still one line, ever" test "$(d_typed exp)" = 1

  # --- Step 7: the audit file ---------------------------------------------------------------
  r_check "audit: routine_create has a call and an outcome under agent-a" \
    d_audit_check 'any(r["client"] == "agent-a" and r["tool"] == "routine_create" and r["phase"] == "call" for r in records) and any(r["client"] == "agent-a" and r["tool"] == "routine_create" and r["phase"] == "outcome" and r.get("outcome") == "ok" for r in records)'
  r_check "audit: the create outcome names the routine id" \
    d_audit_check 'any(r["tool"] == "routine_create" and r["phase"] == "outcome" and r.get("routine") == "r1" for r in records)'
  r_check "audit: nudges are send_input call+outcome pairs by system:routine with routine and created_by" \
    d_audit_check 'any(r["client"] == "system:routine" and r["tool"] == "send_input" and r["phase"] == "call" and r.get("routine") == "r1" and r.get("created_by") == "agent-a" for r in records) and any(r["client"] == "system:routine" and r["phase"] == "outcome" and r.get("outcome") == "ok" for r in records)'
  r_check "audit: as many nudge outcomes as nudge calls" \
    d_audit_check 'sum(1 for r in records if r["client"] == "system:routine" and r["phase"] == "call") == sum(1 for r in records if r["client"] == "system:routine" and r["phase"] == "outcome")'
  r_check "audit: the refused creates were audited too" \
    d_audit_check 'any(r["tool"] == "routine_create" and r["phase"] == "outcome" and str(r.get("outcome", "")).startswith("error:") for r in records)'
  r_check "audit: routine_list wrote no line" \
    d_audit_check 'not any(r["tool"] == "routine_list" and r["phase"] != "denied" for r in records)'
  r_check "audit: the text is nowhere in the file" \
    d_audit_check 'text not in raw and not any("text" in r for r in records)'

  # --- Step 8: the profile loses dynamic_routines -------------------------------------------
  lines_before="$(d_typed dyn)"
  busy_before="$(d_typed busy)"
  d_write_profiles off
  r_check "SIGUSR2 after the profile edit: a new front serves MCP" d_restart
  r_check "dyn: r1 is stopped" d_wait_rfield dyn r1 status stopped 20
  r_check "dyn: the reason is profile_changed" test "$(d_rfield dyn r1 stop_reason)" = profile_changed
  r_check "busy: r1 is stopped" test "$(d_rfield busy r1 status)" = stopped
  r_check "the log says the routine was stopped" r_log_has 'routine r1 stopped \(profile_changed\)'
  sleep 65                                            # more than one grid interval
  r_check "dyn: nothing typed after the profile change" test "$(d_typed dyn)" = "$lines_before"
  r_check "busy: nothing typed after the profile change" test "$(d_typed busy)" = "$busy_before"
  r_check "routine_delete works on a stopped routine" \
    test "$(as_agent rc_delete dyn r1 2>/dev/null | d_py 'd["deleted"]')" = true
  as_agent rc_delete busy r1 >/dev/null 2>&1
  as_agent rc_delete exp r1 >/dev/null 2>&1
  r_check "dyn has no routine left" test "$(as_agent rc_list dyn 2>/dev/null | d_py 'len(d["routines"])')" = 0

  # --- Step 9: the audit file cannot be written ---------------------------------------------
  d_write_profiles on
  r_check "SIGUSR2 with dynamic_routines back: a new front serves MCP" d_restart
  local size need
  size="$(wc -c < "$D_AUDIT" | tr -d ' ')"
  need=$((D_FSIZE - size + 1024))
  head -c "$need" /dev/zero | tr '\0' '\n' >> "$D_AUDIT"      # the file now reaches the limit
  r_check "with the audit file full, routine_create is audit_unavailable" \
    test "$(r_error as_agent rc_create dyn 60 "$D_NUDGE_TEXT")" = audit_unavailable
  r_check "...and nothing was created" \
    test "$(as_agent rc_list dyn 2>/dev/null | d_py 'len(d["routines"])')" = 0
  r_check "routine_list still works" as_agent rc_list dyn

  r_finish
}

if [ "${D_SOURCE_ONLY:-}" != 1 ]; then d_main; fi
