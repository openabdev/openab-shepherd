# OpenAB Shepherd

An MCP server that exposes terminal panes as MCP tools. It listens on loopback only
and requires a bearer token. The panes are run by a terminal backend behind a trait
(see [Terminal backend](#terminal-backend)); the only backend today is herdr-core
(hc). Status: MVP. The 120 unit tests pass on the build host. See `MVP-REPORT.md`.

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
| `src/backend.rs` | The `TerminalBackend` trait and Shepherd's own types: `TerminalInfo`, `ProgramStatus`, `Capabilities`, `InputGuard`, `BackendError`, ... |
| `src/hc/mod.rs` | `HcBackend`: the herdr-core adapter. The only code that knows hc method names, JSON fields, and error kinds |
| `src/hc/rpc.rs` | JSON-RPC 2.0 client over the herdr-core Unix socket |
| `src/hc/fake.rs`, `src/hc/tests.rs` | Test-only: a fake hc socket, and tool tests over the hc wire |
| `src/tools.rs` | The eight tools, their schemas, the safety rules, and the backend calls each one makes |
| `src/dialog.rs` | Text parser for the kiro-cli permission dialog |
| `src/mcp.rs` | MCP message handling |
| `src/http.rs` | Loopback Host check, bearer auth, POST handling |
| `src/testutil.rs` | Test-only helpers: scratch dirs, git init, an in-memory `FakeBackend` |

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

## Tools

Panes are identified by **name**: pass the name `start_pane` returned as `terminal`
to every other tool. Every pane Shepherd starts has one: give `start_pane` a `name`,
or Shepherd picks `pane-` plus 12 hex digits and returns it. The backend's own
reference (herdr-core: its address) stays inside Shepherd, where the pane lock keys
on it; pane objects do not carry it. A pane started outside Shepherd without a name
is listed by `list_panes` with `name: null` and cannot be addressed through Shepherd.

Every tool that takes `terminal` first resolves it with a `get`, and goes on only if
the pane found carries exactly that name. herdr-core's `get` also accepts an address
(`t9`), so this check is what keeps addresses out: a call naming an address, whether
of an unnamed pane or of a named one, fails with `not_found` and nothing else reaches
the backend. Calls after the lookup use the pane's internal id. `send_input`,
`interrupt_pane`, and `answer_dialog` resolve the pane twice (see
[Pane lock](#pane-lock)). The implementing function is in
`src/tools.rs`; the herdr-core method behind each backend call is listed in
[Terminal backend](#terminal-backend).

| MCP tool | Backend calls | Parameters | Result |
|----------|---------------|------------|--------|
| `start_pane` | `run` | `argv` or `line` (exactly one), `cwd`, `name` (optional; generated when missing) | a [pane object](#pane-object) |
| `send_input` | `get` (twice), `send` | `terminal`, `text` | `terminal`, `turn`, `started`, `status` (a state word or null); on refusal, error `pane_blocked` with `kind` |
| `read_output` | `get`, `read` | `terminal`, `lines` (0 to 10000) | `text` (escapes stripped), `complete`, `source` (`screen`, `history`, `stitched`, or `other`), `reason` (why only the screen was read: `blocked`, `other`, or null) |
| `get_status` | `get` | `terminal` | a [pane object](#pane-object) plus `terminal` |
| `wait_status` | `get`, `wait`, then `get` if it ended blocked without a kind | `terminal`, `timeout` (1 to 1800 s, default 30) | `terminal`, `outcome`, `turn_complete`, `ended`, `reason`, `status` (a state word), `kind`, `exit_code`, `turn`, and `status_now` when the extra `get` ran |
| `list_panes` | `list` | `all` | `panes` (pane objects), `hidden_finished` (finished panes left out; null if the backend does not say) |
| `interrupt_pane` | `get` (twice), `interrupt` | `terminal`, `timeout` (1 to 60 s, optional) | `terminal`, `key` (the stop key the backend chose: `esc`, `ctrl+c`, or `other`), `outcome` (`stopped`, `exited`, `still_working`, `unknown`, or `other`) |
| `answer_dialog` | `get`, `read`, `keys` | `terminal`, `choice` (`allow_once` or `deny_once`) | `terminal`, `choice`, `selected`, `title`, `command`, `options`, `moves`, `keys_result` (`status`, `turn`) |

Tool results carry `structuredContent` and a text copy. Unknown tool names are
JSON-RPC errors.

Every word field in a result (`status`/`state`, `source`, `reason`, `key`,
`outcome`) uses Shepherd's own words only. A word the backend sends that Shepherd
does not know is reported as `other`, never verbatim. `wait_status`'s `reason` is
null when the backend gave none.

### Pane object

`start_pane`, `get_status`, and each entry of `list_panes` return the same shape.
This replaced the herdr-core terminal object that earlier versions passed through
unchanged (a breaking change of the result shape; no client depended on it).

| Field | Meaning |
|-------|---------|
| `name` | The pane's name. Null only for a pane started outside Shepherd without one |
| `live` | False once the program has ended |
| `exit_code` | The exit code once ended, else null |
| `status` | `{state, kind}`, or null if the program never reported a status (or has ended). `state` is `idle`, `working`, `done`, `blocked`, `error`, or `other` (a word Shepherd does not know). `kind` is set only for `blocked`: `permission`, `question`, `auth`, or `other` (a dialog kind Shepherd does not know) |
| `reported` | Whether the program ever reported a status (OSC 7501) |
| `turn` | Goes up each time the status leaves `working` |
| `status_ms` | When the status last changed, ms since the epoch |
| `argv`, `cwd` | What it was started with, when the backend says |

### Errors

A tool failure returns `isError: true`. The text is `<code>: <message>`, and
`structuredContent` always holds `error` (the code) and `message`, plus details that
depend on the code (`terminal`, `kind`, `next`, ...). Codes and messages are
Shepherd's own. The backend's original error text is kept only in `backend_detail`,
for debugging; do not parse it.

| Code | When |
|------|------|
| `invalid_input` | An argument failed validation (nothing was sent), or the backend refused the request's parameters |
| `pane_blocked` | Typing was refused because the program is blocked on a dialog; nothing was typed. `kind` names the dialog (for example `permission`) when known; `next` says to read the screen, then use `answer_dialog` |
| `not_found` | No pane is named `terminal` (a backend address is not a name); for `start_pane`, the program or working directory was not found |
| `name_taken` | `start_pane` with a name that is already in use (or, rarely, a generated name that collided; retry) |
| `status_mismatch` | A backend-side input condition did not hold; nothing was sent |
| `pane_finished` | The pane's program has ended |
| `terminal_changed` | The name pointed at a different pane while waiting for the pane lock; nothing was sent |
| `refused` | `answer_dialog` refused; see [answer_dialog](#answer_dialog) |
| `unavailable` | The backend cannot be reached |
| `timeout` | The backend did not answer in time |
| `unsupported` | The backend cannot do this (for example a screen read from a backend without screen text) |
| `backend_error` | Any other backend failure, or a reply Shepherd cannot use |

### wait_status outcome

Branch on `outcome`:

| `outcome` | When | `turn_complete` |
|-----------|------|-----------------|
| `done` | `ended`, `reason: "status"`, `status: "done"` | true |
| `exited` | `ended`, `reason: "exit"` | true |
| `blocked` | `ended`, `reason: "status"`, `status: "blocked"`. `kind` comes from a follow-up `get` when the wait did not report it (herdr-core's wait never does) | false |
| `timeout` | `ended: false`, `reason: "timeout"`: a check-in, the pane is still running | false |
| `other` | anything else (`idle`, `error`, a message to the caller, `not_started`). Read `status` and `reason` | false |

`turn` advances on blocked too, so a changed `turn` is not a completion signal. A
wait reply the backend cannot read (herdr-core: no boolean `ended`) is reported as
error `backend_error`, never as idle. If the follow-up `get` shows the pane is no
longer blocked, `kind` is null and `status_now` shows the current status.

### send_input

`idle` and `done` are both sendable (kiro-cli stays in `done` after a turn). The tool
never forces input. While the program is blocked, nothing is typed and the tool
returns error `pane_blocked` with `kind` (the dialog, for example `permission`) and
`next`, which says to read the screen and use `answer_dialog`.

Who refuses depends on the backend's `conditional_input` capability. herdr-core has
it: its send refuses a blocked program in the same step as the write. If herdr-core's
refusal does not name the dialog, Shepherd reads the pane once more for `kind` (and
uses the status read before the send only if that read fails). A
backend without it gets the check from Shepherd: the status read inside the pane lock
must not be `blocked`, then the text is sent, still inside the lock.

### answer_dialog

1. Take the pane lock, then `get`: refuse unless `status.state` is `blocked` and
   `status.kind` is `permission`. Record the `turn` number.
2. `read` (screen): parse the dialog. kiro-cli 3.1.0 (V3) reports
   `blocked`/`permission` before it has drawn the dialog, so the tool re-reads the
   screen every 200 ms (`RENDER_POLL_INTERVAL`) for up to 3 s (`RENDER_WAIT`, 16
   reads) until the dialog parses. Before each re-read it calls `get` and
   refuses, sending nothing, if the pane is no longer `blocked` with kind
   `permission` or the `turn` changed; the refusal says which. If the dialog still
   does not parse after the wait, it refuses, sending nothing, and says it timed out
   waiting for the dialog to render. The dialog needs a line ending in
   `requires approval`, the next line as `<tool> → <command>`, then exactly four
   options in this order: `Allow once`, `Deny once`, `Create allow rule...`,
   `Create deny rule...`, with one `❯` cursor. Anything else is refused.
3. Move the cursor with `up` or `down` (`keys`). `allow_once` from the default
   cursor needs no moves; `deny_once` needs one `down`. After moving, the status must
   still be `blocked`.
4. Read the screen again, up to 5 times 200 ms apart, until the same dialog shows the
   cursor on the target option. A different dialog stops it at once.
5. `get` again: refuse unless the pane is still `blocked` with kind `permission`
   and `turn` is unchanged. The refusal says which of the two changed.
6. Only then press `enter`. The lock is released when the call returns.

Every key is sent with an `InputGuard` (state `blocked`, kind `permission`, the
recorded turn). A backend with the `conditional_keys` capability enforces it in the
same step as the key press, and Shepherd then skips step 5. herdr-core lacks it, so
for herdr-core step 5 runs as described.

If a check fails, the tool sends nothing more, never sends `enter`, and returns
error `refused` with `structuredContent` holding `reason`, `seen` (the status or the
last 40 screen lines), and `keys_sent`. The rule-creating options cannot be chosen.
Follow a successful answer with `wait_status`.

What this guarantees, and what it does not:

- **Serialized against other Shepherd calls.** No other `answer_dialog`,
  `send_input`, or `interrupt_pane` on the same pane can run between the first
  `get` and `enter`. A second `answer_dialog` waits, then finds the pane no
  longer blocked and is refused.
- **Re-checked just before Enter.** Status, kind, and turn are read again after the
  cursor is confirmed.
- **No herdr-core guard on the key.** herdr-core 0.1.19's key press has no status
  or turn condition, so `conditional_keys` is false for it. The re-check and the
  `enter` are therefore two calls. A person at the terminal or another herdr-core
  client on the same host can still change the dialog between them, and `enter`
  would then select whatever is under the cursor. The design accepts this because
  Shepherd is meant to be the only client of the socket.

### Pane lock

`send_input`, `interrupt_pane`, and `answer_dialog` hold a per-pane lock for their
whole sequence. The lock is keyed by the pane's backend id (internal, never returned), so two calls
that resolve to the same pane share one lock. To find the id, the tool resolves the
name (a `get`, checked as above) before taking the lock, then resolves it again inside
the lock and refuses with `terminal_changed` (sending nothing) if the name now
resolves to a different pane. `start_pane` takes no lock: `run`
refuses a name that is taken, so it never types into an existing pane.
Read-only tools (`read_output`, `get_status`, `wait_status`, `list_panes`) take no
lock. A call waiting for the lock waits as long as the holder runs (an
`interrupt_pane` holds it for up to its timeout; an `answer_dialog` holds it for up
to the 3 s render wait plus its other steps: up to 0.8 s of cursor confirmation and
each backend call).

## Terminal backend

`src/backend.rs` defines a synchronous `TerminalBackend` trait: `capabilities`, `run`,
`get`, `list`, `read`, `send`, `keys`, `wait`, and `interrupt`. The tools in
`src/tools.rs` call only the trait, and every type they see (`TerminalInfo`,
`ProgramStatus`, `ScreenText`, `WaitResult`, `Key`, `BackendError`, ...) is Shepherd's
own. The safety rules stay in Shepherd whatever the backend: input validation, the
pane lock, refusing to type into a blocked pane, and `answer_dialog`'s checks.

`Capabilities` says what a backend can also do itself, so Shepherd can choose a
native call or its own fallback:

| Flag | When true | When false | herdr-core |
|------|-----------|------------|------------|
| `conditional_input` | `send` enforces its `InputGuard` (refuse while blocked) in the same step as the write | Shepherd checks the status under the pane lock, then sends | true |
| `conditional_keys` | `keys` enforces its `InputGuard` (state, kind, turn) with the key press | Shepherd re-checks status, kind, and turn before `enter` | false |
| `client_command` | `run` takes a caller-chosen `argv` or `line` | `start_pane` returns `unsupported` | true |
| `client_cwd` | `run` takes a caller-chosen `cwd` | `start_pane` with `cwd` returns `unsupported` | true |
| `screen_text` | `read` returns rendered screen text | `read_output` and `answer_dialog` return `unsupported` | true |

`RunSpec` also has a `profile` field for backends that only start operator-defined
launch profiles; Shepherd does not set it yet, and the hc backend refuses it.

The only backend is `HcBackend` (`src/hc/`), over the herdr-core socket
(`HC_SOCKET`). Trait call to herdr-core method:

| Trait call | herdr-core method | Notes |
|------------|-------------------|-------|
| `run` | `terminal.run` | `argv` or `line`, `cwd`, `name` |
| `get` | `terminal.get` | |
| `list` | `terminal.list` | `all` |
| `read` | `terminal.read` | `lines` |
| `send` | `terminal.send` | never `force`; refuses while blocked |
| `keys` | `terminal.keys` | no condition |
| `wait` | `terminal.wait` | socket timeout is the wait plus 15 s |
| `interrupt` | `terminal.interrupt` | |

hc error kinds map to Shepherd codes: `not_found` and `machine_unknown` to
`not_found`, `name_taken` to `name_taken`, `blocked` to `pane_blocked`,
`status_mismatch` to `status_mismatch`, `finished` to `pane_finished`,
`invalid_params` to `invalid_input`, `unavailable`, `unreachable` (or a socket that
cannot be reached) to `unavailable`, a reply timeout to `timeout`, and everything else
(`spawn_failed`, `config`, transport or protocol failures, ...) to `backend_error`.
`src/hc/error_kinds.json` lists every error kind in the herdr-core 0.1.19 schema and
the kinds each adapter method can return; the tests check that each one has an
expected Shepherd code and reaches MCP as that code. Dialog kinds in hc statuses
are normalized the same way: `permission`, `question` and `auth` pass through, and
any other word reaches callers as `other`. Regenerate it when the
supported herdr-core version changes.

## Input rules

Applied to every string field that reaches the backend (`line`, each `argv` element,
`cwd`, `name`, `text`, and `terminal`):

1. Refuse any control character except TAB: C0 controls other than TAB, DEL, and C1
   (U+0080 to U+009F). This includes newline, carriage return, and ESC. The text is
   not altered. The tool returns an error that names the code point (for example
   `U+000A`), and nothing is sent to the backend.
2. Trim surrounding whitespace.
3. Reject an empty result.
4. Reject a result over **4096 bytes** of UTF-8.

`name` must also match herdr-core's naming rule: 1 to 32 characters of `a-z`, `0-9`,
`-`, `_`, `.`.

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

The unit tests need no herdr-core daemon. The tool tests over the hc wire use a fake
Unix socket in `/tmp`, the backend-neutral tests use an in-memory `FakeBackend`, and
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
  conditional key press (`conditional_keys` is false). A person at the terminal or
  another herdr-core client can still move or replace the dialog between that
  re-check and `enter`.
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
- **One backend.** herdr-core is the only `TerminalBackend`. The trait was shaped
  for a second one (openab-pty), which first needs screen text, program status,
  and a control API of its own.
- **Integration.** The unit tests use a fake socket or a fake backend. The operator
  ran the live herdr-core checks on the MVP build (`MVP-REPORT.md`); they have not
  been re-run since the result shapes changed.
