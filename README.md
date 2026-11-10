# OpenAB Shepherd

An MCP server that exposes herdr-core terminal panes as MCP tools. It listens on
loopback only and requires a bearer token. Status: MVP. The 91 unit tests pass on the
build host. Live herdr-core integration checks are NOT RUN. See `MVP-REPORT.md`.

## Language: Rust

- The user chose Rust for the MVP. The project rule is that the laptop never runs
  `cargo` or `rustc`, so all compilation happens on the remote build host (a remote build host).
- The release build is one native binary with no interpreter or virtualenv to
  manage on the machine that runs herdr-core.
- `serde_json` parses and emits JSON-RPC. `tiny_http` provides the HTTP listener.
  Two direct dependencies in total.

## MCP SDK or hand-written: hand-written

The MCP surface this server needs is small: `initialize`, `notifications/initialized`,
`ping`, `tools/list`, and `tools/call`, over HTTP POST with JSON replies. `src/mcp.rs`
and `src/http.rs` implement it, and unit tests exercise both.

An official Rust MCP SDK was not used. I did not verify its current API offline, and
its streaming and session features are unused here. Replacing this layer with an SDK
later is a contained change.

## Layout

| File | Purpose |
|------|---------|
| `src/main.rs` | Arguments, startup, exit codes |
| `src/config.rs` | Bind check, token source and location check, socket path |
| `src/input.rs` | Input rules for text sent to panes |
| `src/ansi.rs` | Escape-sequence filter for pane output |
| `src/herdr.rs` | JSON-RPC 2.0 client over the herdr-core Unix socket |
| `src/tools.rs` | The eight tools, their schemas, and the methods each one calls |
| `src/dialog.rs` | Text parser for the kiro-cli permission dialog |
| `src/mcp.rs` | MCP message handling |
| `src/http.rs` | Loopback Host check, bearer auth, POST handling |
| `src/testutil.rs` | Test-only helpers: scratch dirs, fake socket, git init |

## Run

Build on the remote build host, then copy the release binary back (see Build offload).
Create the token file outside any git repository:

```sh
umask 077
openssl rand -hex 32 > <token-file>
chmod 600 <token-file>
```

Start the server. `HC_SOCKET` is the herdr-core socket path. The server has no default
for it.

The command below is documentation for the operator. The operator runs it by hand,
outside the agent's command guard. No script in this crate runs it.

```sh
OPENAB_SHEPHERD_TOKEN_FILE=<token-file> \
HC_SOCKET=<hc-socket-path> \
./openab-shepherd --port 8765
```

- Endpoint: `http://127.0.0.1:8765/mcp`. Every request sends `Authorization: Bearer <token>`.
- `--host` accepts only `127.0.0.1` or `::1`. Any other value, including `0.0.0.0`
  and `localhost`, exits with status 2 before anything listens.
- No token, both token variables set, a token shorter than 16 characters, or a
  missing `HC_SOCKET` also exit with status 2. The reason goes to stderr.
- Token variables: `OPENAB_SHEPHERD_TOKEN_FILE` (preferred) or `OPENAB_SHEPHERD_TOKEN`.
  Use one of them, not both.

### Token file rules

- The file must be mode 0600 or stricter (no group or other bits).
- It must not be inside the project folder (the folder the server starts from),
  and it must not be inside any git repository that the project folder or the
  token's own folder belongs to. Repository roots come from
  `git rev-parse --show-toplevel`. If git cannot be run, or fails for another
  reason, the server refuses to start.

### herdr-core socket

- `HC_SOCKET` is the override. The herdr-core binary names it in its own error text.
- `hc --help` names `~/.config/herdr-core/config.toml` as the settings file. It does
  not show `HC_SESSION` or a `socket` key. This server reads neither, and whether
  either one sets the socket is unverified. The operator sets `HC_SOCKET` explicitly.
- The default socket path is unknown. The server never guesses one.

## Tool-to-method map

Most tools send one herdr-core JSON-RPC method. `wait_status` adds a `terminal.get`
when the wait ends blocked, and `answer_dialog` uses three methods. `send_input`,
`interrupt_pane`, and `answer_dialog` first resolve the pane with two `terminal.get`
calls (see [Pane lock](#pane-lock)). The implementing
function is in `src/tools.rs`. Parameter names are as written there. `hc.openrpc.json` was checked
against them in an earlier step.

| MCP tool | herdr-core method | Parameters sent | Result returned |
|----------|-------------------|-----------------|-----------------|
| `start_pane` | `terminal.run` | `argv` or `line` (exactly one), `cwd`, `name` | the terminal object, with `address` |
| `send_input` | `terminal.get` (twice), `terminal.send` | `terminal`, `text` (never `force`, never `if`) | the send result (`turn`, `started`, `status`, `wait`); on refusal, an error with `kind` and `hint` |
| `read_output` | `terminal.read` | `terminal`, `lines` (0 to 10000) | `text` (escapes stripped), `complete`, `how`, `why` |
| `get_status` | `terminal.get` | `terminal` | `terminal`, `status`, `reported`, `state`, `turn`, `status_ms` |
| `wait_status` | `terminal.wait`, then `terminal.get` if it ended blocked | `terminal`, `timeout` (1 to 1800 s, default 30) | `terminal`, `outcome`, `turn_complete`, `ended`, `reason`, `status`, `kind`, `exit_code`, `turn`, and `status_now` when blocked |
| `list_panes` | `terminal.list` | `all` | the list result (`terminals`, `hidden_finished`, `machines`) |
| `interrupt_pane` | `terminal.get` (twice), `terminal.interrupt` | `terminal`, `timeout` (1 to 60 s, optional) | `key`, `outcome` |
| `answer_dialog` | `terminal.get`, `terminal.read`, `terminal.keys` | `terminal`, `choice` (`allow_once` or `deny_once`) | `choice`, `selected`, `title`, `command`, `options`, `moves`, `keys_result` |

Tool results carry `structuredContent` and a text copy. A tool failure returns
`isError: true` with the reason. `send_input` and `answer_dialog` failures also carry
`structuredContent` with the details. Unknown tool names are JSON-RPC errors.

### wait_status outcome

Branch on `outcome`:

| `outcome` | When | `turn_complete` |
|-----------|------|-----------------|
| `done` | `ended`, `reason: "status"`, `status: "done"` | true |
| `exited` | `ended`, `reason: "exit"` | true |
| `blocked` | `ended`, `reason: "status"`, `status: "blocked"`. `kind` comes from a follow-up `terminal.get` (`status.kind`), because `terminal.wait` does not report it | false |
| `timeout` | `ended: false`, `reason: "timeout"`: a check-in, the pane is still running | false |
| `other` | anything else (`idle`, `error`, a message to the caller, `not_started`). Read `status` and `reason` | false |

`turn` advances on blocked too, so a changed `turn` is not a completion signal. A
herdr-core reply without a boolean `ended` is reported as an error, never as idle.
If the follow-up `terminal.get` shows the pane is no longer blocked, `kind` is null and
`status_now` shows the current status.

### send_input

`idle` and `done` are both sendable (kiro-cli stays in `done` after a turn). The tool
sends neither `force` nor `if`. While the program is blocked, herdr-core refuses and
types nothing. The tool returns `isError: true` with `structuredContent` holding
herdr-core's `kind` (`blocked`), `hint`, and `message`, plus `next`, which says to read
the screen and use `answer_dialog`.

### answer_dialog

1. Take the pane lock, then `terminal.get`: refuse unless `status.state` is
   `blocked` and `status.kind` is `permission`. Record the `turn` number.
2. `terminal.read` (screen): parse the dialog. kiro-cli 3.1.0 (V3) reports
   `blocked`/`permission` before it has drawn the dialog, so the tool re-reads the
   screen every 200 ms (`RENDER_POLL_INTERVAL`) for up to 3 s (`RENDER_WAIT`, 16
   reads) until the dialog parses. Before each re-read it calls `terminal.get` and
   refuses, sending nothing, if the pane is no longer `blocked` with kind
   `permission` or the `turn` changed; the refusal says which. If the dialog still
   does not parse after the wait, it refuses, sending nothing, and says it timed out
   waiting for the dialog to render. The dialog needs a line ending in
   `requires approval`, the next line as `<tool> → <command>`, then exactly four
   options in this order: `Allow once`, `Deny once`, `Create allow rule...`,
   `Create deny rule...`, with one `❯` cursor. Anything else is refused.
3. Move the cursor with `up` or `down` (`terminal.keys`). `allow_once` from the default
   cursor needs no moves; `deny_once` needs one `down`. After moving, the status must
   still be `blocked`.
4. Read the screen again, up to 5 times 200 ms apart, until the same dialog shows the
   cursor on the target option. A different dialog stops it at once.
5. `terminal.get` again: refuse unless the pane is still `blocked` with kind
   `permission` and `turn` is unchanged. The refusal says which of the two changed.
6. Only then press `enter`. The lock is released when the call returns.

If a check fails, the tool sends nothing more, never sends `enter`, and returns
`isError: true` with `structuredContent` holding `reason`, `seen` (the status or the
last 40 screen lines), and `keys_sent`. The rule-creating options cannot be chosen.
Follow a successful answer with `wait_status`.

What this guarantees, and what it does not:

- **Serialized against other Shepherd calls.** No other `answer_dialog`,
  `send_input`, or `interrupt_pane` on the same pane can run between the first
  `terminal.get` and `enter`. A second `answer_dialog` waits, then finds the pane no
  longer blocked and is refused.
- **Re-checked just before Enter.** Status, kind, and turn are read again after the
  cursor is confirmed.
- **No herdr-core guard on the key.** herdr-core 0.1.19's `terminal.keys` takes only
  `terminal`, `keys`, and `caller`; it has no status or turn condition (only
  `terminal.send` has `if`, limited to `idle` or `working`). The re-check and the
  `enter` are therefore two calls. A person at the terminal or another herdr-core
  client on the same host can still change the dialog between them, and `enter`
  would then select whatever is under the cursor. The design accepts this because
  Shepherd is meant to be the only client of the socket.

### Pane lock

`send_input`, `interrupt_pane`, and `answer_dialog` hold a per-pane lock for their
whole sequence. The lock is keyed by the pane's address (`t7`), so a name and its
address share one lock. To find the address, the tool calls `terminal.get` before
taking the lock, then calls it again inside the lock and refuses (sending nothing) if
the name now resolves to a different address. `start_pane` takes no lock:
`terminal.run` refuses a name that is taken, so it never types into an existing pane.
Read-only tools (`read_output`, `get_status`, `wait_status`, `list_panes`) take no
lock. A call waiting for the lock waits as long as the holder runs (an
`interrupt_pane` holds it for up to its timeout; an `answer_dialog` holds it for up
to the 3 s render wait plus its other steps: up to 0.8 s of cursor confirmation and
each herdr-core call).

## Input rules

Applied to every string field that reaches herdr-core (`line`, each `argv` element,
`cwd`, `name`, `text`, and `terminal`):

1. Refuse any control character except TAB: C0 controls other than TAB, DEL, and C1
   (U+0080 to U+009F). This includes newline, carriage return, and ESC. The text is
   not altered. The tool returns an error that names the code point (for example
   `U+000A`), and nothing is sent to herdr-core.
2. Trim surrounding whitespace.
3. Reject an empty result.
4. Reject a result over **4096 bytes** of UTF-8.

`name` must also match herdr-core's rule: 1 to 32 characters of `a-z`, `0-9`, `-`,
`_`, `.`.

The cleaned value is what gets sent. For `argv`, that means each element is trimmed,
so a leading or trailing space inside an argument is removed.

## Output rules

`read_output` removes CSI sequences (`ESC [ ...`), OSC sequences (`ESC ] ...` ending
in BEL or ST, including OSC 7501 status reports), DCS, SOS, PM, and APC strings,
two-byte and nF escapes, and lone ESC bytes. An unterminated sequence at the end of the
text removes the rest of the text. Newlines and tabs are kept.

## Authentication and network

- Host header: checked first. It must name a loopback address (`127.0.0.1`, `[::1]`,
  or `localhost`, with or without a port). Any other value gets HTTP 403, with no
  token check.
- Bearer token: compared in constant time. A missing or wrong token gets HTTP 401.
- Only `POST /mcp` is served. `GET` gets 405. Notifications get 202.
- The listener binds the validated loopback address only. The server does not run
  `tailscale serve`, Funnel, or any other public listener. The herdr-core socket is
  never exposed.
- Request bodies are capped at 1 MiB, read before the token check. Request bodies
  are not logged.

## Tests

On the remote build host:

```sh
cd <repo>/openab-shepherd
export PATH=$HOME/.cargo/bin:$PATH
cargo test
cargo build --release
```

The unit tests need no herdr-core daemon. They use a fake Unix socket in `/tmp`, and
the config tests run `git init` in a scratch directory, so `git` must be installed.

## Build offload

The laptop never runs `cargo` or `rustc`. Sync the crate to the build host with
`rsync -a --delete --exclude target/`, which keeps the remote build cache. Build and
test there, then copy `target/release/openab-shepherd` back.

## Known limits (MVP scope)

- **No multi-client authorization.** Any holder of the token has full control of every
  pane.
- **Permission dialogs: text parsing, one layout.** `answer_dialog` parses the dialog
  from screen text. It is tested only against kiro-cli 3.1.0 (V3 engine) shell
  permission dialogs, using the screen captured by the probe. Other tools (fs_write,
  MCP), other programs, other versions, and `kind` values `question` or `auth` are
  untested or refused. `force` is never sent.
- **Residual race on Enter.** `answer_dialog` is serialized against other Shepherd
  calls and re-checks status and turn just before `enter`, but herdr-core has no
  conditional key press. A person at the terminal or another herdr-core client can
  still move or replace the dialog between that re-check and `enter`.
- **Auto-allowing agents never block.** User Kiro agents whose `allowedTools` include
  `@builtin` (the default agent does) run builtin tools without a dialog, so the pane
  never reports `blocked` and `answer_dialog` has nothing to answer. `--trust-tools=`
  does not override this on V3. Testing the dialog path needs an agent that trusts no
  tools.
- **ANSI-only output filter.** TUI redraw noise is not removed. Screen captures can
  contain repeated or partial frames. The 8-bit CSI introducer (U+009B) is removed,
  but the 8-bit OSC introducer (U+009D) is not.
- **Done versus idle.** kiro-cli 3.1.0 stays in `done` after a turn, not `idle`
  (measured by the OSC 7501 probe). A `wait_status` on such a pane ends with
  `outcome: "done"`; the next prompt goes `done` → `working`. A program that never reports status counts as
  working until it exits, so `wait_status` on it returns only on exit or timeout.
- **Trimmed argv.** Argument text is trimmed by the input rules, so leading and
  trailing spaces inside an `argv` element are lost.
- **Unverified Grok Bot MCP client behavior.** How that client connects and calls
  these tools is untested.
- **Plain-mode framing.** The client sends no `hello`. The schema describes plain
  mode as one message per line. Whether herdr-core requires `hello` first is
  unverified until the integration checks run.
- **Stateless HTTP.** No SSE stream and no session ids. Clients that expect server
  push or sessions will not get them.
- **Socket path.** The default is unknown. `HC_SOCKET` is required.
- **Token file check-then-read.** The mode and location are checked, then the file
  is read. Someone who can replace files in the token's folder could swap the file
  between the two steps. The 0600 mode and a folder outside the repository are the
  defense.
- **Integration not run.** The unit tests use a fake socket. The live herdr-core
  checks are NOT RUN. `MVP-REPORT.md` lists the commands the operator runs outside the
  guard.
