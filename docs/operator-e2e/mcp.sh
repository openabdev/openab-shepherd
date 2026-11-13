#!/usr/bin/env bash
# Source this file: `source mcp.sh`. Thin MCP client for OpenAB Shepherd.
#
# Protocol (src/http.rs, src/mcp.rs): POST http://127.0.0.1:PORT/mcp, JSON-RPC 2.0,
# `Authorization: Bearer <token>`, no session id, plain JSON replies (no SSE);
# a notification gets HTTP 202 with no body. Tool results carry `structuredContent`.
#
# The token is read from the token file and handed to curl on stdin (curl -K -),
# so it never appears in argv, the process list or the output. JSON is built and
# parsed with python3; no value is spliced into a shell string.

SHEPHERD_URL="${SHEPHERD_URL:-http://127.0.0.1:8765/mcp}"
SHEPHERD_TOKEN_FILE="${SHEPHERD_TOKEN_FILE:-$HOME/.config/shepherd-e2e/token}"
SHEPHERD_WORK="${SHEPHERD_WORK:-/tmp/shepherd-e2e}"
_MCP_ID=0

# _post JSON  -> prints the reply body; returns 0 on HTTP 200/202, 1 otherwise
# (the status and body go to stderr on failure). Override this for offline tests.
_post() {
  local body="$1" out http_code
  out="$(mktemp)" || return 1
  http_code="$(
    {
      printf 'header = "Authorization: Bearer %s"\n' "$(tr -d '\n\r ' < "$SHEPHERD_TOKEN_FILE")"
    } | curl -sS -K - -o "$out" -w '%{http_code}' \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        --data-binary "$body" "$SHEPHERD_URL"
  )" || { rm -f "$out"; echo "curl failed (is the server running on $SHEPHERD_URL?)" >&2; return 1; }
  cat "$out"
  rm -f "$out"
  case "$http_code" in
    200|202) return 0 ;;
    401) echo "HTTP 401: wrong or missing bearer token" >&2 ;;
    403) echo "HTTP 403: Host header not accepted (use 127.0.0.1, [::1] or localhost)" >&2 ;;
    *) echo "HTTP $http_code" >&2 ;;
  esac
  return 1
}

_next_id() { _MCP_ID=$((_MCP_ID + 1)); echo "$_MCP_ID"; }

mcp_init() {
  local id body reply
  id="$(_next_id)"
  body="$(python3 -c 'import json,sys
print(json.dumps({"jsonrpc":"2.0","id":int(sys.argv[1]),"method":"initialize",
 "params":{"protocolVersion":"2025-06-18","capabilities":{},
           "clientInfo":{"name":"shepherd-e2e","version":"0"}}}))' "$id")" || return 1
  reply="$(_post "$body")" || return 1
  printf '%s' "$reply" | python3 -c 'import json,sys
r=json.load(sys.stdin)["result"]
print("initialized:", r["serverInfo"]["name"], r["serverInfo"]["version"], "protocol", r["protocolVersion"])' || return 1
  _post '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null
}

# mcp_tool NAME [key=value ...]   values are strings; use key:=JSON for numbers/booleans.
# Prints structuredContent as JSON. Exit status 1 when the tool reported isError.
mcp_tool() {
  local name="$1"; shift
  local id body reply
  id="$(_next_id)"
  body="$(python3 -c 'import json,sys
name, rid, *kv = sys.argv[1:]
args = {}
for item in kv:
    k, v = item.split("=", 1)
    if k.endswith(":"):
        args[k[:-1]] = json.loads(v)
    else:
        args[k] = v
print(json.dumps({"jsonrpc":"2.0","id":int(rid),"method":"tools/call",
                  "params":{"name":name,"arguments":args}}))' "$name" "$id" "$@")" || return 1
  reply="$(_post "$body")" || return 1
  printf '%s' "$reply" | python3 -c 'import json,sys
r=json.load(sys.stdin)
if "error" in r:
    print(json.dumps(r["error"], indent=2)); sys.exit(1)
res=r["result"]
print(json.dumps(res.get("structuredContent"), indent=2, ensure_ascii=False))
sys.exit(1 if res.get("isError") else 0)'
}

start_pane()    { mcp_tool start_pane "profile=$1" "cwd=$2" "name=$3"; }
send_input()    { mcp_tool send_input "terminal=$1" "text=$2"; }
wait_status()   { mcp_tool wait_status "terminal=$1" "timeout:=${2:-60}"; }
read_output()   { mcp_tool read_output "terminal=$1" "lines:=${2:-0}"; }
get_status()    { mcp_tool get_status "terminal=$1"; }
answer_dialog() { mcp_tool answer_dialog "terminal=$1" "choice=$2"; }   # allow_once | deny_once
remove_pane()   { mcp_tool remove_pane "terminal=$1" "timeout:=${2:-5}"; }
list_panes()    { mcp_tool list_panes "all:=${1:-false}"; }

# read_screen NAME: print the screen text of a pane as plain text.
read_screen() { read_output "$1" 0 | python3 -c 'import json,sys; print(json.load(sys.stdin)["text"])'; }

# expect PATH=VALUE ...   Reads JSON on stdin, echoes it, then prints a PASS/FAIL
# line per expectation. PATH is dotted (status.state); VALUE is the JSON text of
# the field (idle, true, null, 0). Exit status 1 if any expectation fails.
expect() {
  python3 -c 'import json,sys
text = sys.stdin.read(); print(text.rstrip())
doc = json.loads(text); bad = 0
for spec in sys.argv[1:]:
    path, want = spec.split("=", 1)
    cur = doc
    for part in path.split("."):
        cur = cur.get(part) if isinstance(cur, dict) else None
    got = cur if isinstance(cur, str) else json.dumps(cur)
    ok = (got == want)
    bad += (not ok)
    print(("  PASS " if ok else "  FAIL ") + f"{path}: expected {want}, got {got}", file=sys.stderr)
sys.exit(1 if bad else 0)' "$@"
}

# Marker checks (the verdict comes from files, never from model text).
expect_file()    { if [ -e "$1" ]; then echo "  PASS exists:  $1" >&2; else echo "  FAIL missing: $1" >&2; return 1; fi; }
expect_no_file() { if [ ! -e "$1" ]; then echo "  PASS absent:  $1" >&2; else echo "  FAIL exists:  $1" >&2; return 1; fi; }
