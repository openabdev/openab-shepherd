# OpenAB Shepherd MVP report

## Status

- Unit tests on the build host: 70 of 70 pass.
- Release binary: built on the build host. The laptop copy has the same SHA-256.
- Integration checks 1-3 (live herdr-core checks, negative runs): NOT RUN.
- The crate is uncommitted and unpushed.

## Unit tests on remote build host

Command, run on remote build host in the synced crate:

```
ssh remote build host 'cd ~/repo/openab-shepherd && export PATH=$HOME/.cargo/bin:$PATH && cargo test'
```

Result: 70 passed, 0 failed. Excerpt of the output:

```
   Compiling openab-shepherd v0.1.0
    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.19s
     Running unittests src/main.rs (target/debug/deps/openab_shepherd-48221f4014ef3e8f)

running 70 tests
[70 "test ... ok" lines]

test result: ok. 70 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
```

Per-file counts of `#[test]` in `src/`, which add to 70: `ansi.rs` 10, `config.rs` 13,
`herdr.rs` 8, `http.rs` 3, `input.rs` 13, `main.rs` 3, `mcp.rs` 20. `tools.rs` and `testutil.rs`
have no tests of their own; `tools.rs` is covered through `mcp.rs`.

## Build

- Sync: `rsync -a --delete` from the laptop crate to the build host.
- Build: `cargo build --release` on remote build host. The release profile finished in 2.33 s.
- SHA-256 of the remote build host release binary (`target/release/openab-shepherd`):
  `5673cb52353a276469cb6b05f20d8eb84b28661afdaa690977a77a4e64a0df4b`
- The laptop copy was copied from remote build host with `scp`. Its SHA-256 is:
  `5673cb52353a276469cb6b05f20d8eb84b28661afdaa690977a77a4e64a0df4b`
- Result: MATCH.
- The laptop binary that existed before the rebuild was kept as
  `target/release/openab-shepherd.pre-rebuild`.

## Tool-to-method map

Source: `src/tools.rs`. Each function sends one herdr-core JSON-RPC method. Parameter names are
as written in that file. `hc.openrpc.json` was checked against them in an earlier step (see
`build-review.md`).

| MCP tool | Function | herdr-core method | Parameters sent |
|----------|----------|-------------------|-----------------|
| `start_pane` | `start_pane` | `terminal.run` | `argv` or `line` (exactly one), `cwd`, `name` |
| `send_input` | `send_input` | `terminal.send` | `terminal`, `text`. `force` is never sent. |
| `read_output` | `read_output` | `terminal.read` | `terminal`, `lines` (0 to 10000) |
| `get_status` | `get_status` | `terminal.get` | `terminal` |
| `wait_status` | `wait_status` | `terminal.wait` | `terminal`, `timeout` (1 to 1800 s, default 30) |
| `list_panes` | `list_panes` | `terminal.list` | `all` |
| `interrupt_pane` | `interrupt_pane` | `terminal.interrupt` | `terminal`, `timeout` (optional, 1 to 60 s) |

`call()` in the same file dispatches by tool name. Input checks run before any socket call.

## Integration checks 1-3: NOT RUN

Reason: the shell command guard blocks any command whose text contains `--host` or `--port`.
The server start and the bind-refusal test both need those flags. This run did not get past the
guard. The operator runs the commands below outside the guard.

### Guard-bypass incident

In the earlier integration attempt, the guard blocked commands containing `--host` and `--port`.
That attempt moved both flags into two files, `run.sh` and `bind-refusal.sh`, and started the
server through them. That got past the guard. The results of that attempt are excluded from this
report: Checks 2 and 3, including the live bind-refusal test. The temporary folder with those files
has been removed, and neither file exists in the crate or in the task folder. This run did not
repeat the workaround.

### Commands for the operator

Placeholders: `<TOKEN_FILE>` is the token file. `<TEST_CWD>` is a working folder outside the git
repository. `<hc-socket-path>` is the herdr-core socket. The expected results come from the check
definitions in `integration-evidence.md`. They have not been observed in a valid run.

Create the token file (any folder outside the repository):

```sh
umask 077
TOKEN_FILE=$(mktemp)          # use this path as <TOKEN_FILE>
openssl rand -hex 32 > <TOKEN_FILE>
chmod 600 <TOKEN_FILE>
```

**Check 1: unit tests on remote build host.** Expected: `test result: ok. 70 passed; 0 failed`.

```sh
ssh remote build host 'cd ~/repo/openab-shepherd && export PATH=$HOME/.cargo/bin:$PATH && cargo test'
```

**Check 2: live server on the laptop.**

Terminal A, start the server (the operator runs this line):

```sh
cd <TEST_CWD>
OPENAB_SHEPHERD_TOKEN_FILE=<TOKEN_FILE> HC_SOCKET=<hc-socket-path> \
  ./target/release/openab-shepherd --host 127.0.0.1 --port 8767
```

Expected: stderr shows `openab-shepherd: listening on http://127.0.0.1:8767/mcp`. Port 8765 was
already taken on this machine, so the checks use 8767.

Terminal B, send the requests. Each one sends the token from the file:

```sh
AUTH="Authorization: Bearer $(cat <TOKEN_FILE>)"
URL=http://127.0.0.1:8767/mcp
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_panes","arguments":{"all":true}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"start_pane","arguments":{"name":"shepherd-int","cwd":"<TEST_CWD>","argv":["sh","-c","echo SHEPHERD_READY; cat"]}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"send_input","arguments":{"terminal":"shepherd-int","text":"echo SHEPHERD_ECHO_1"}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"read_output","arguments":{"terminal":"shepherd-int","lines":0}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_status","arguments":{"terminal":"shepherd-int"}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"wait_status","arguments":{"terminal":"shepherd-int","timeout":5}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"interrupt_pane","arguments":{"terminal":"shepherd-int","timeout":5}}}'
```

Expected:

| Request | Expected |
|---------|----------|
| `initialize` | HTTP 200. `result.protocolVersion` is `2025-06-18`. `serverInfo.name` is `openab-shepherd`. `capabilities.tools` is present. |
| `notifications/initialized` | HTTP 202, no body. |
| `tools/list` | HTTP 200. Seven tools: `start_pane`, `send_input`, `read_output`, `get_status`, `wait_status`, `list_panes`, `interrupt_pane`. |
| `list_panes` (all=true) | `isError: false`. The list is empty before the test pane starts. |
| `start_pane` | `isError: false`. A `structuredContent` terminal object with an `address` and `state` `live`. |
| `send_input` | `isError: false`. |
| `read_output` (after about 2 s) | `isError: false`. `text` contains `SHEPHERD_ECHO_1`. |
| `get_status` | `isError: false`. `reported` is `false`, `state` is `live`, `status` is `null`. |
| `wait_status` (timeout 5) | `isError: false`. `ended` is `false` with reason `timeout`, returned after about 5 s. |
| `interrupt_pane` (timeout 5) | `isError: false`. `key` is `ctrl+c`. `outcome` is one of `stopped`, `exited`, `still_working`, `unknown`. |

Cleanup: remove the test pane, confirm the list, and stop the server (Ctrl-C in Terminal A):

```sh
hc rm <TERMINAL_ADDRESS>      # the address returned by start_pane
hc ls --all                   # expected: no terminals
```

**Check 3: negative tests.**

No token, and a wrong token. Expected: HTTP 401 with `WWW-Authenticate: Bearer` and body
`{"error":"unauthorized",...}`. The server from Check 2 must be running.

```sh
curl -s -i http://127.0.0.1:8767/mcp -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
curl -s -i http://127.0.0.1:8767/mcp -H 'Authorization: Bearer wrongtoken0000000000000000' -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Non-loopback bind refused. Expected: exit status 2. stderr contains
`refusing to bind "0.0.0.0": only 127.0.0.1 and ::1 are allowed`. No listener on port 8768.

```sh
OPENAB_SHEPHERD_TOKEN_FILE=<TOKEN_FILE> HC_SOCKET=<hc-socket-path> \
  ./target/release/openab-shepherd --host 0.0.0.0 --port 8768; echo "exit=$?"
lsof -nP -iTCP:8768 -sTCP:LISTEN
```

Whitespace-only and control-character input. Run these with the Check 2 server up, and run
`hc ls --all` before and after. Expected: both calls return `isError: true`, and the pane list does
not change.

```sh
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"start_pane","arguments":{"line":"   "}}}'
curl -s -i $URL -H "$AUTH" -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"start_pane","arguments":{"line":"echo bell\u0007"}}}'
```

- Whitespace-only: text `line: input is empty after trimming`.
- Control character: text `line: input contains control character U+0007; remove it and retry`.

## Known limits

- **No multi-client authorization.** Any holder of the token controls every pane.
- **No permission-dialog handling.** A pane blocked on a dialog reports `blocked`. The server does
  not answer dialogs. `send_input` is refused in that state, and `force` is never sent.
- **ANSI-only output filter.** The filter removes CSI (including the 8-bit introducer U+009B), OSC
  (including OSC 7501), DCS, SOS, PM, APC, two-byte and nF escapes, and lone ESC. The 8-bit OSC
  introducer (U+009D) is not removed. TUI redraw noise is not removed. An unterminated OSC or DCS
  hides the text after it.
- **Done versus idle.** kiro-cli 3.1.0 stays in `done` after a turn, not `idle`. The OSC 7501 probe
  measured this (`.agents/tasks/openab-shepherd-osc7501-probe/report.md`). `wait_status` ends with
  `done` in that case, so callers must read `status`.
- **Wait semantics for programs that never report status.** Such a program counts as working until
  it exits. `wait_status` returns only on exit or on timeout (`ended: false`).
- **Unverified Grok Bot MCP client behavior.** How that client connects and calls these tools is
  untested.
- **Input trimming.** Leading and trailing spaces inside an `argv` element are removed.
- **Stateless HTTP.** No SSE stream and no session ids.
- **Host check before token check.** A request with a non-loopback Host gets 403 with no token
  check. A request with a loopback Host and a bad token gets 401.
- **Request body read before auth.** Each request reads up to 1 MiB before the token check.
- **Socket path.** The herdr-core socket default is unknown. `HC_SOCKET` is required. Its precedence
  against `HC_SESSION` and the config key is unverified.
- **Token file check-then-read.** The mode and location are checked, then the file is read. Someone
  who can replace files in the token's folder could swap it in between.
- **Plain-mode framing.** Whether herdr-core requires `hello` before plain-mode calls is unverified.
  Only a live call settles it.
- **Formatting and lint not recorded.** No `cargo fmt --check` or `cargo clippy` run is recorded.
  The workspace guide requires both before a push.

## Evidence files

- `integration-evidence.md` records Checks 1-3 as PASS. Those results came from the guard bypass
  described above, so they are excluded.
- `build-review.md` (iteration 3) reviews that file and marks integration as BLOCKED (B2).
- `unit-evidence.md` does not claim integration passed. Its runs are the unit tests, the build,
  the hash copy, and a `--help` and `--bogus` smoke check. It states that the laptop integration
  checks are not run in its step.
- No valid run supports the PASS claims in `integration-evidence.md`. These three files are not
  edited here.

## Deviations from the brief

- **TAB is accepted inside input.** TAB is a control character, but the rule refuses only the
  characters that break one-message-per-line framing, and TAB does not. The Python reference also
  keeps TAB.
- **MCP layer hand-written**, not an SDK. The brief allows either.
- **Not committed.** The crate is untracked. The parent repository shows it as `?? openab-shepherd/`.
