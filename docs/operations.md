# OpenAB Shepherd: operations reference

This is the operator reference: how to run the server, its environment variables,
launch profiles, the tools, the backends, the Host check and the token rules. For
the overview, see the [README](../README.md).

Shepherd is an MCP server that exposes terminal panes as MCP tools. It listens on
loopback only and requires a bearer token. The panes are run by a terminal backend
behind a trait (see [Terminal backend](#terminal-backend)): the native core is the
only backend. Status: MVP. Test and probe results are in
[`MVP-REPORT.md`](../MVP-REPORT.md), a dated record rather than a current status.

## Language: Rust

- The user chose Rust for the MVP. The project rule is that the laptop never runs
  `cargo` or `rustc`, so all compilation happens on a remote build host.
- The release build is one native binary with no interpreter or virtualenv to
  manage on the machine that runs it.
- `serde_json` parses and emits JSON-RPC. `tiny_http` provides the HTTP listener.
  The native core adds `libc` and `vt100` (pinned to `=0.18.1`). Four direct
  dependencies in total.

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
| `src/main.rs` | Arguments, startup, exit codes (a thin binary over `src/lib.rs`) |
| `src/config.rs` | Bind check, token source and location check, backend choice, socket path |
| `src/profiles.rs` | Launch profiles: parsing, the cwd allowlist, env sources (`keychain:`, `env:`) |
| `src/input.rs` | Input rules for text sent to panes |
| `src/ansi.rs` | Escape-sequence filter for pane output |
| `src/backend.rs` | The `TerminalBackend` trait and Shepherd's own types: `TerminalInfo`, `ProgramStatus`, `Capabilities`, `InputGuard`, `BackendError`, ... |
| `src/native/mod.rs` | `NativeBackend`: pane table, reader and reaper threads, guarded writes, the trait impl |
| `src/native/pty.rs`, `term.rs`, `status.rs`, `keys.rs`, `procs.rs` | PTY spawn and child env; the `vt100` emulator and query replies; OSC 7501 parsing and turns; key encoding; the process table and descendant tree used by cleanup |
| `src/native/tests.rs` | Test-only: native tests with real programs |
| `examples/native_probe.rs` | The real-Kiro probe for the native backend |
| `examples/native_leak_check.rs` | Real-Kiro leak check: removes a pane and verifies no descendant process survives (issue #10) |
| `src/tools.rs` | The nine tools, their schemas, the safety rules, and the backend calls each one makes |
| `src/watchdog.rs` | The per-pane watchdog state and its nudge decision (the tools run its thread) |
| `src/dialog.rs` | Text parser for the kiro-cli permission dialog |
| `src/devin_dialog.rs` | Text parser for the Devin CLI permission dialog |
| `src/opencode_dialog.rs` | Text parser for the OpenCode permission dialog and the styled-cell highlight check |
| `src/agent.rs` | The agents a profile can name (`kiro`, `devin`, `opencode`), their dialects, and `permission_mode` |
| `src/panelock.rs` | The per-pane lock that serializes calls typing into one pane |
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

Start the server. It runs panes on its own native terminal core, the only backend
(see [Native backend](#native-backend)). `OPENAB_SHEPHERD_BACKEND` may be unset,
empty or `native`; it needs nothing else.

The command below is documentation for the operator. The operator runs it by hand,
outside the agent's command guard. No script in this crate runs it.

```sh
OPENAB_SHEPHERD_TOKEN_FILE=<token-file> \
OPENAB_SHEPHERD_PROFILES=<profiles-file> \
./openab-shepherd --port 8765
```

Without `OPENAB_SHEPHERD_PROFILES` the server has no launch profiles and allows no
arbitrary commands, so `start_pane` starts nothing (see
[Launch profiles](#launch-profiles)).

- Endpoint: `http://<loopback-address>:8765/mcp`, where `<loopback-address>` is the
  address given to `--host` (IPv4 loopback by default). Every request sends
  `Authorization: Bearer <token>`.
- `--host` accepts only the literal IPv4 loopback address (the default) or the literal
  IPv6 loopback address. Any other value, including the all-interfaces wildcard
  address and the name `localhost`, exits with status 2 before anything listens.
- No token, both token variables set, a token shorter than 16 characters, an
  `OPENAB_SHEPHERD_BACKEND` that is set to anything other than empty or `native`
  (including `hc`, which was removed) also exit with status 2. The reason goes to
  stderr.
- Token variables: `OPENAB_SHEPHERD_TOKEN_FILE` (preferred) or `OPENAB_SHEPHERD_TOKEN`.
  Use one of them, not both.
- A launch profiles file that is unreadable, inside a git repository, or invalid
  also exits with status 2.
- An invalid `OPENAB_SHEPHERD_ALLOWED_HOSTS` entry also exits with status 2 (see
  [Behind tailscale serve](#behind-tailscale-serve)).
- The startup line names the backend: `(backend: native)`.

### Token file rules

- The file must be mode 0600 or stricter (no group or other bits).
- It must not be inside the project folder (the folder the server starts from),
  and it must not be inside any git repository that the project folder or the
  token's own folder belongs to. Repository roots come from
  `git rev-parse --show-toplevel`. If git cannot be run, or fails for another
  reason, the server refuses to start.

### Launch profiles

`start_pane` starts what the operator's launch profiles allow. The file is JSON, named
by `OPENAB_SHEPHERD_PROFILES`, and follows the token file's location rule: outside
the project folder and outside any git repository (symlinks resolved first). It must
also be a regular file owned by the user Shepherd runs as and writable by no one else
(`chmod 600` or `644`); Shepherd checks and reads the same open file. Otherwise
Shepherd refuses to start.

Upgrading: `start_pane` without a profiles file now refuses every start with
`profile_required`. Create a profiles file with the programs
you start, or set `"allow_arbitrary_command": true` in it to keep caller-chosen
`argv`/`line` (without credentials).

```json
{
  "allow_arbitrary_command": false,
  "profiles": [
    {
      "name": "kiro",
      "argv": ["kiro-cli", "chat"],
      "extra_args": ["--trust-workspace"],
      "cwds": ["/path/to/work"],
      "env": { "KIRO_API_KEY": "keychain:<keychain-item>" }
    },
    {
      "name": "devin",
      "agent": "devin",
      "argv": ["devin"],
      "cwds": ["/path/to/work"]
    }
  ]
}
```

- `name`: the pane-name rules (1-32 of `a-z`, `0-9`, `-`, `_`, `.`); unique.
- `argv`, then `extra_args` (optional): the fixed program and arguments. A caller
  cannot change or add to them. `argv[0]` is a program name or an absolute path, not a
  relative path. A name is looked up when the pane starts, before any credential is
  read, in Shepherd's `PATH` (default `/usr/bin:/bin:/usr/sbin:/sbin`), skipping empty
  and relative entries such as `.`, so the caller's `cwd` never decides which
  executable gets the profile's environment. The pane runs that absolute path (its
  `argv` shows it); no executable found fails `start_pane` with `not_found`.
- `cwds`: 1 to 32 absolute directories, which must exist at startup. A caller's
  `cwd` must resolve (symlinks and `..` resolved) to one of them or a directory
  below one; without `cwd` the first is used. The pane runs in the resolved path.
- `env` (optional, at most 32): variables for this profile's panes only. A name is
  `[A-Z_][A-Z0-9_]*` and may not be `TERM`, `PATH`, `DYLD_*`, `LD_*`,
  `OPENAB_SHEPHERD_*` or `HC_*`. A value
  is one of:
  - `keychain:<service>`: read when the pane starts with
    `/usr/bin/security find-generic-password -s <service> -w` (clean environment,
    10 s limit). macOS only; elsewhere the file is refused. Create the item with
    `security add-generic-password -s <service> -a "$USER" -w` (it prompts).
  - `env:<NAME>`: copied from Shepherd's own environment when the pane starts. It may
    not name `OPENAB_SHEPHERD_*` or `HC_*`.
  - anything else: a literal, for **non-secret settings only**; it sits in the file.

  A value read from a source goes only into that child's environment. It is never
  written to the config, state, logs, argv, tool results or errors; an error names
  the variable and its source, not the value. A source that cannot be read fails
  `start_pane` with `profile_unavailable`, and nothing starts.
- `allow_arbitrary_command` (default false): when true, `start_pane` also takes a
  caller-chosen `argv` or `line` (and `env`). Such a pane never gets a profile's
  environment, whatever program it runs.
- `agent` (optional, default `kiro`): `kiro`, `devin` or `opencode`. It decides which permission
  dialog `answer_dialog` reads for panes this profile starts, and, for `devin`, the
  launch flag below. For `opencode`, `answer_dialog` sends Esc for `deny_once` and presses Enter for `allow_once` only after
  it verifies the highlight from styled cells (see the OpenCode section of the README), and its profiles must use `permission_mode` `normal`. A pane started any other way has no agent, so `answer_dialog`
  refuses it with no keys sent. Panes started before this Shepherd process have no
  record of their agent, so they are refused too.
  A profile whose `argv` or `extra_args` names `opencode` (case-insensitive substring) and contains an auto-approve spelling (`--auto`, `autoaccept`, `auto-accept`) is refused at load. This prevents accidental configuration; it does not defend against a renamed binary, a wrapper script, or `allow_arbitrary_command`.
  `send_input` refuses `/yolo` (trimmed, case-insensitive) on a Devin pane, because it may switch to Bypass mode. This is a name-based check against accidental use, not a defense against a different spelling or a wrapper.
- `permission_mode` (optional, default `normal`): what the agent may do without
  asking.
  - `normal`: a `devin` pane runs `devin --permission-mode auto`. Shell commands and
    writes ask for approval. A `kiro` pane gets no flag; its arguments are its own
    `argv`.
  - `accept-edits`: `devin` only, `devin --permission-mode accept-edits`. Refused for
    `kiro`, because no Kiro mode was measured.
  - `bypass`: `devin` only, `devin --permission-mode dangerous`, which approves every
    tool. It is never a default: a profile must say `"permission_mode": "bypass"`.
    For `kiro` it needs `"allow_bypass": true`, and Shepherd adds no flag. Kiro has no
    bypass flag that was measured, so the Kiro arguments in `argv` still decide trust.
- `allow_bypass` (optional, default false): lets a `kiro` profile declare
  `permission_mode` `bypass`. It has no effect on a `devin` profile.
- `watchdog` (optional): Shepherd's own liveness check on this profile's panes,
  checked every 5 s. A coding CLI's main session sometimes ends a turn with work
  left and stops — not working, not blocked, waiting on nothing. When a pane's
  turn has finished (`done`; for `opencode`, `idle` after at least one turn, the
  same rule as `wait_status`) and the turn number then sits untouched for
  `idle_timeout_secs`, the watchdog types `nudge_text` into the pane through the
  same path and refusal rules as `send_input`, and logs the nudge (pane, turn,
  reason). It never types into a blocked pane and never answers a dialog.
  Whether the remaining work is worth doing stays the client's judgment; the
  watchdog is off unless a profile sets it, and it watches only panes this
  Shepherd process started from the profile (a pane name reused by any other
  start drops the registration). Fields:
  - `idle_timeout_secs` (required): 10 to 86400.
  - `nudge_text` (required): the text typed, under the `send_input` input rules
    (trimmed, at most 4096 bytes, no control characters except TAB). A bare
    confirmation or menu answer is refused when the profiles file is read:
    a single character, a number of one or two digits, or a word such as
    `yes`, `ok`, `enter`, `allow`, `always`, `deny`, `reject`, `bypass` or
    `trust`. Use a sentence that tells the agent what to do.
  - `cooldown_secs` (optional, default `idle_timeout_secs`): 0 to 86400, the
    least time between two nudges to the same pane.
  - `max_consecutive_nudges` (optional, default 3): 1 to 100. After that many
    nudges without progress the watchdog did not cause, the watchdog stops
    for the pane and logs that it gave up. The cap counts nudges sent. The
    turns a nudge itself causes — up to 3 turn advances after each nudge,
    whether the agent answers with one turn or a follow-up as well — do not
    count as progress and do not clear the count or the stop; a larger
    advance, or one with no nudge outstanding, does. A nudge that produces
    no turn counts like any other. Failed or refused nudge attempts also count
    toward the cap and are spaced by the cooldown, so a pane that refuses
    every nudge is not retried on every check. Without a `todo_file`, every
    finished turn is a nudge candidate, so a pane whose work is in fact
    complete will still be nudged up to this cap; set a `todo_file` when
    that distinction matters.
  - `todo_file` (optional): a path relative to the pane's working directory,
    with no `..`. When set, a nudge also needs an unchecked item — a line whose
    first non-blank characters are `- [ ]` — in that file. No unchecked item
    (every item checked, or no items at all) counts as finished work, and the
    pane is never nudged; a file that cannot be read is never nudged either.
    The match is a plain line scan that skips fenced code blocks (a line
    starting with three backticks or tildes opens and closes one). The file is read defensively: it must be a regular file, a
    file over 256 KiB counts as unreadable, and the transition to
    unreadable is logged once. On Unix the working directory is the
    boundary: it is canonicalized and opened as a directory handle once,
    when the pane is registered (so a working directory reached through a
    symlinked ancestor still reads, and swapping it or an ancestor for a
    symlink later is not followed), and a symlink in
    any component of the `todo_file` path below it counts as unreadable.
    On non-Unix the read is a plain open and follows symlinks.
  Watchdog profiles are meant for unattended panes. The watchdog types
  into a pane on its own: if a nudge's text is pasted but Enter is then
  refused (the native backend's second guarded step), the text is left
  on the pane's input line, the watchdog does not retry it — a retry
  would paste a second copy — and it stops for that pane until a turn
  advance it did not cause. A person at the pane in the meantime could
  submit that pasted text by pressing Enter themselves. A watched pane
  whose program has ended is forgotten, not re-checked on every pass.
  The watchdog thread serves every watched pane, so it waits at most 2 s
  for a pane's lock: a pane another call holds for longer is skipped for
  that check, with nothing sent and nothing counted.
  The watchdog's audit trail is Shepherd's stderr log: every nudge (with
  the pane's idle seconds), every give-up and every failed attempt is a
  line there, and nowhere else — a client cannot tell a watchdog nudge
  from its own input except that the pane's turn counter advances, which a
  watchdog nudge does like any typed prompt. Registrations live only in
  memory: a Shepherd restart forgets every watched pane, and a pane is
  watched again only when a watchdog profile starts it again.
- A `devin` profile's directory must already be trusted in Devin. On a fresh directory
  Devin asks "Do you trust the authors of this directory?". Shepherd never answers
  that prompt. `start_pane` reads the screen for about 3 s after the start, closes the
  pane and returns `trust_prompt` when it shows. Trust the directory once (run `devin`
  there and choose `1`), then start the pane again. A prompt that appears after the
  window is not detected; `answer_dialog` then finds no dialog and refuses.
- A `devin` profile may not put `--permission-mode` (or `--permission-mode=`) in
  `argv` or `extra_args`, or set `DEVIN_PERMISSION_MODE` in `env`. The mode is set
  in one place only, so the Devin config file cannot change it: Devin's flag takes
  precedence over its own config, and `normal` passes `auto` explicitly for that reason.
  It may not set `--respect-workspace-trust` either (any value): that flag turns off the
  trust check Shepherd relies on for `trust_prompt`.
- Unknown fields are refused, so a typo cannot silently drop a restriction.
- `list_panes` returns `profiles` (each profile's `name` and `cwds`, never its argv or
  env) and `allow_arbitrary_command`, so a caller can see what it may start.
- Programs a profile's pane runs (Kiro's tools, for example) inherit its
  environment. A profile's credential is as exposed as anything that program does.

For how the operator signs in to the coding CLI or supplies its key, see
[credentials.md](credentials.md).

## Tools

Panes are identified by **name**: pass the name `start_pane` returned as `terminal`
to every other tool. Every pane Shepherd starts has one: give `start_pane` a `name`,
or Shepherd picks `pane-` plus 12 hex digits and returns it. The backend's own
reference (an internal id) stays inside Shepherd, where the pane lock keys
on it; pane objects do not carry it. A pane started outside Shepherd without a name
is listed by `list_panes` with `name: null` and cannot be addressed through Shepherd.

Every tool that takes `terminal` first resolves it with a `get`, and goes on only if
the pane found carries exactly that name. A call naming an internal id, whether of an
unnamed pane or of a named one, fails with `not_found` and nothing else reaches the
backend. Calls after the lookup use the pane's internal id. `send_input`,
`interrupt_pane`, `answer_dialog`, and `remove_pane` resolve the pane twice (see
[Pane lock](#pane-lock)). The implementing function is in
`src/tools.rs`; the backend calls are the `TerminalBackend` trait methods listed in
[Terminal backend](#terminal-backend).

| MCP tool | Backend calls | Parameters | Result |
|----------|---------------|------------|--------|
| `start_pane` | `run` | `profile` (a [launch profile](#launch-profiles)), `cwd` (one the profile allows), `name` (optional; generated when missing); or, only with `allow_arbitrary_command`, `argv` or `line` (exactly one), `cwd`, `name`, `env` (optional object, this pane only) | a [pane object](#pane-object) |
| `send_input` | `get` (twice), `send` | `terminal`, `text` | `terminal`, `turn`, `started`, `status` (a state word or null); on refusal, error `pane_blocked` with `kind` |
| `read_output` | `get`, `read` | `terminal`, `lines` (0 to 10000) | `text` (escapes stripped), `complete`, `source` (`screen`, `history`, `stitched`, or `other`), `reason` (why only the screen was read: `blocked`, `other`, or null) |
| `get_status` | `get` | `terminal` | a [pane object](#pane-object) plus `terminal` |
| `wait_status` | `get`, `wait`, then `get` if it ended blocked without a kind | `terminal`, `timeout` (1 to 1800 s, default 30) | `terminal`, `outcome`, `turn_complete`, `ended`, `reason`, `status` (a state word), `kind`, `exit_code`, `turn`, and `status_now` when the extra `get` ran |
| `list_panes` | `list` | `all` | `panes` (pane objects), `hidden_finished` (finished panes left out; null if the backend does not say), `live_panes` (running panes now), `live_pane_limit` (how many may run at once: 8 on the native backend; null for a backend with no limit), `profiles` (`name` and `cwds` of each launch profile), `allow_arbitrary_command` |
| `interrupt_pane` | `get` (twice), `interrupt` | `terminal`, `timeout` (1 to 60 s, optional) | `terminal`, `key` (the stop key the backend chose: `esc`, `ctrl+c`, or `other`), `outcome` (`stopped`, `exited`, `still_working`, `unknown`, or `other`) |
| `answer_dialog` | `get`, `read`, `keys` | `terminal`, `choice` (`allow_once` or `deny_once`) | `terminal`, `choice`, `selected`, `title`, `command`, `options`, `moves`, `keys_result` (`status`, `turn`) |
| `remove_pane` | `get` (twice), `remove` | `terminal`, `timeout` (0 to 60 s, default 2: the grace before the kill) | `name`, `was_live` (the program was running), `stopped` (`exited`, `killed`, `other`, or null when it had already finished), `exit_code` (128 + the signal number when a signal ended it; null when unknown). See [remove_pane](#remove_pane) |

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
| `pane_blocked` | Typing was refused because the program is blocked on a dialog; nothing was typed. `kind` names the dialog (for example `permission`) when known; `next` says to read the screen, then use `answer_dialog`; for `kind=auth`, `next` says to tell the operator, who must log in |
| `not_found` | No pane is named `terminal` (a backend address is not a name); for `start_pane`, the program or working directory was not found |
| `name_taken` | `start_pane` with a name that is already in use (or, rarely, a generated name that collided; retry) |
| `status_mismatch` | A backend-side input condition did not hold; nothing was sent |
| `pane_finished` | The pane's program has ended |
| `terminal_changed` | The name pointed at a different pane while waiting for the pane lock; nothing was sent |
| `refused` | `answer_dialog` refused; see [answer_dialog](#answer_dialog) |
| `unavailable` | The backend cannot be reached |
| `timeout` | The backend did not answer in time |
| `unsupported` | The backend cannot do this (for example a screen read from a backend without screen text) |
| `profile_required` | `start_pane` without `profile` while arbitrary commands are not allowed; nothing started |
| `profile_unavailable` | A launch profile's `keychain:` or `env:` source could not be read; nothing started |
| `trust_prompt` | A `devin` pane stopped at "Do you trust the authors of this directory?" within about 3 s of starting. Shepherd never answers it: the pane is closed, and the error names the prompt. Trust the directory once in Devin, then start the pane again |
| `backend_error` | Any other backend failure, or a reply Shepherd cannot use |

### wait_status outcome

Branch on `outcome`:

| `outcome` | When | `turn_complete` |
|-----------|------|-----------------|
| `done` | `ended`, `reason: "status"`, `status: "done"` | true |
| `exited` | `ended`, `reason: "exit"` | true |
| `blocked` | `ended`, `reason: "status"`, `status: "blocked"`. `kind` comes from a follow-up `get` when the wait did not report it (a backend's wait may not report it) | false |
| `cancelled` | `ended`, `reason: "status"`, `status: "idle"`, after `answer_dialog` on this process answered this pane's dialog (a Devin `deny_once` or Esc returns to idle, not done) | false |
| `timeout` | `ended: false`, `reason: "timeout"`: a check-in, the pane is still running | false |
| `other` | anything else (`idle`, `error`, a message to the caller, `not_started`). Read `status` and `reason` | false |

`turn` advances on blocked too, so a changed `turn` is not a completion signal. A
wait reply the backend cannot read (for example, no boolean `ended`) is reported as
error `backend_error`, never as idle. If the follow-up `get` shows the pane is no
longer blocked, `kind` is null and `status_now` shows the current status.

### send_input

`idle` and `done` are both sendable (kiro-cli stays in `done` after a turn). The tool
never forces input. While the program is blocked, nothing is typed and the tool
returns error `pane_blocked` with `kind` (the dialog, for example `permission`) and
`next`, which says to read the screen and use `answer_dialog`. For `kind=auth` the
CLI is not logged in: `next` says to tell the operator, and `answer_dialog` is not the way.

Who refuses depends on the backend's `conditional_input` capability. The native
backend has it: its send refuses a blocked program in the same step as the write. If
the refusal does not name the dialog, Shepherd reads the pane once more for `kind` (and
uses the status read before the send only if that read fails). A
backend without it would get the check from Shepherd: the status read inside the pane lock
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
   waiting for the dialog to render. The dialect comes from the pane's `agent`:
   - `kiro`: the dialog needs a line ending in `requires approval`, the next line as
     `<tool> → <command>`, then exactly four options in this order: `Allow once`,
     `Deny once`, `Create allow rule...`, `Create deny rule...`, with one `❯` cursor.
   - `devin`: the dialog needs a line reading `Running command` (optionally preceded by
     one `⏺` and spaces), then `$ <command>` (optionally preceded by `└ `),
     then three option rows in this order: `Allow` (its scope selector may follow on
     the same row), `Bypass mode`, `Deny`, with one `❭` on the highlighted row.
   - `opencode` (OpenCode v2.2.7): the dialog needs a line starting `△ Permission required`,
     then `$ <command>` or `→ Edit <path>` (an edit may show a diff preview), then one row of
     `Allow once`, `Always allow`, `Reject` in that order, followed by the `ctrl+f fullscreen`
     hint. `deny_once` skips steps 3 and 4 below: after the re-check it sends `esc` (equal to
     `Reject`) once, never `enter`, `right` or any move key, and never selects `Always allow`.
     `allow_once` skips steps 3 and 4 too, and replaces step 6: the backend reads the styled
     cells (foreground, background, reverse; internal only) and presses `enter` in one step,
     under the status, kind and turn guard (`TerminalBackend::keys_if_screen`). It presses only
     when the same text checks pass and `Allow once` has a background that differs from both
     other labels while those two share one background (a relative rule, compared as colour
     values without converting between palette and RGB, so it works with palette or
     truecolor); all three equal, the highlight on another option, any other pattern, a label
     with mixed backgrounds, a default background on `Allow once`, reverse video on any label,
     text changed since the first read, a screen mid synchronized update, or a backend without
     styled cells all refuse with no key sent. Not measured: other themes, and the foreground
     colour is not checked.

   Anything else is refused.
3. Move the cursor. For `kiro`, `up` or `down` keys (`keys`) go out in one batch:
   `allow_once` from the default cursor needs no moves; `deny_once` needs one `down`.
   For `devin`, one key goes out at a time, and the screen is read after each key. The
   highlight must be on the next row before another key is sent, so it never passes
   the target. `deny_once` from `Allow` sends `down`, `down` and stops on `Deny`.
   The highlight must start on `Allow`, or the tool sends `esc` and refuses. `up` is
   never sent.
   After moving, the status must still be `blocked`.
4. Read the screen again, up to 5 times 200 ms apart, until the same dialog shows the
   cursor on the target option. A different dialog stops it at once.
5. `get` again: refuse unless the pane is still `blocked` with kind `permission`
   and `turn` is unchanged. The refusal says which of the two changed.
6. Only then press `enter`. The lock is released when the call returns.

Every key is sent with an `InputGuard` (state `blocked`, kind `permission`, the
recorded turn). A backend with the `conditional_keys` capability enforces it in the
same step as the key press, and Shepherd then skips step 5. The native backend has
it. A backend without it would run step 5 as described.

If a check fails, the tool never sends `enter`, and returns error `refused` with
`structuredContent` holding `reason`, `seen` (the status or the last 40 screen lines),
and `keys_sent`. For `kiro`, it sends nothing more. For `devin`, when the pane is still
blocked on this turn's permission dialog, it sends `esc`, which cancels the request,
and then refuses; the cancel is recorded for `wait_status`. The rule-creating options
cannot be chosen. A Devin dialog never selects `Bypass mode`: the target is `Allow` or
`Deny` only. Follow a successful answer with `wait_status`.

Devin answers: `allow_once` presses `enter` on `Allow` (the scope defaults to `Once`).
It refuses, sending `esc`, when the Allow row shows another scope than `Once`.
`deny_once` moves to `Deny` with one `down` at a time and presses `enter`. After each `down` it re-reads the screen, up to 5 reads 200 ms apart, until the highlight is on the next row (Devin redraws it a moment late); if it is not there after 5 reads, or lands on another row, it sends `esc` and refuses. The command does not run, and the
pane goes to `idle`, not `done`; `wait_status` reports that as `cancelled`. Esc has
the same end state.

OpenCode reports no `done`: a turn ends `working`, `clear`, `idle`. After `allow_once`
the command runs and `wait_status` reports that idle (after a turn) as `done`. After
`deny_once` (Esc) the idle is `cancelled`. An allow clears an earlier deny's mark in
the same turn, so the last answer decides, and a new turn starts with no mark.

Supported Devin layouts: Devin CLI 3000.12.43 (measured in a plain PTY) and
3000.12.57 (measured in an operator end-to-end run; it draws `⏺ Running command`,
`└ $ <command>` and a wider scope selector). Other versions refuse until they are
measured. The parser is tested on fixtures built from those two layouts.


What this guarantees, and what it does not:

- **Serialized against other Shepherd calls.** No other `answer_dialog`,
  `send_input`, or `interrupt_pane` on the same pane can run between the first
  `get` and `enter`. A second `answer_dialog` waits, then finds the pane no
  longer blocked and is refused.
- **Re-checked just before Enter.** Status, kind, and turn are read again after the
  cursor is confirmed.
- **Guarded key.** The native backend has `conditional_keys`: `enter` carries the
  status, kind and turn guard and is checked in the same step as the key press, and no
  other client can attach to a native pane.

### remove_pane

Closes a pane and forgets it, so callers can free a live slot (the native backend
runs at most 8 panes, and finished panes keep their screen memory until removed).

1. Take the [pane lock](#pane-lock). A call already holding it (an `answer_dialog`,
   say) finishes first; then the pane is read again, and the removal acts on what it
   finds then. If the name is gone by then, the result is `not_found`.
2. If the program is running, stop it with signals, not keys: SIGHUP and SIGTERM to
   its process group and to every descendant of the program (native: also those
   that moved to their own process group or session, such as kiro-cli's node
   `acp-server.js`), up to `timeout` seconds (default 2) for them to exit, then
   SIGKILL to the group and every descendant left. The pane's output is still read
   while they stop, so a program that writes as it exits is not stuck. The leader is
   reaped before the call returns. `stopped` is `exited` when the leader was gone by
   the end of the timeout and `killed` when it had to be killed; `exit_code` is the
   leader's code, or 128 + the signal (137 after the kill). Keys were rejected for
   this step: a program may ignore Esc or Ctrl-C, or sit in a dialog, while signals
   reach every process. To let the program end its turn cleanly, call
   `interrupt_pane` first. A finished pane has already had its leftovers ended when
   its program exited (see [Process groups](#native-backend)); `stopped` is then null
   and `exit_code` is the code it ended with.
3. Remove the pane. `list_panes` (even with `all: true`) no longer shows it, every
   later call naming it gets `not_found`, and `start_pane` can use the name again.

A `send_input`, `interrupt_pane` or `answer_dialog` that was waiting for the lock
gets `not_found` once the removal is done, and nothing is typed. A `wait_status` in
flight on the pane (it takes no lock) returns `outcome: "exited"`.

### Pane lock

`send_input`, `interrupt_pane`, `answer_dialog`, and `remove_pane` hold a per-pane lock for their
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
each backend call; a `remove_pane` holds it for up to its timeout plus about 2 s).

## Terminal backend

`src/backend.rs` defines a synchronous `TerminalBackend` trait: `capabilities`, `run`,
`get`, `list`, `read`, `send`, `keys`, `wait`, `interrupt`, and `remove`. The tools in
`src/tools.rs` call only the trait, and every type they see (`TerminalInfo`,
`ProgramStatus`, `ScreenText`, `WaitResult`, `Key`, `BackendError`, ...) is Shepherd's
own. The safety rules stay in Shepherd whatever the backend: input validation, the
pane lock, refusing to type into a blocked pane, and `answer_dialog`'s checks.

`Capabilities` says what a backend can also do itself, so Shepherd can choose a
native call or its own fallback:

| Flag | When true | When false | native |
|------|-----------|------------|--------|
| `conditional_input` | `send` enforces its `InputGuard` (refuse while blocked) in the same step as the write | Shepherd checks the status under the pane lock, then sends | true |
| `conditional_keys` | `keys` enforces its `InputGuard` (state, kind, turn) with the key press | Shepherd re-checks status, kind, and turn before `enter` | true |
| `client_command` | `run` takes a caller-chosen `argv` or `line` | `start_pane` returns `unsupported` | true |
| `client_cwd` | `run` takes a caller-chosen `cwd` | `start_pane` with `cwd` returns `unsupported` | true |
| `client_env` | `run` takes extra environment variables for that run | `start_pane` with `env` returns `unsupported` | true |
| `screen_text` | `read` returns rendered screen text | `read_output` and `answer_dialog` return `unsupported` | true |
| `program_status` | the backend reports the program's OSC 7501 status and turns | informational | true |
| `history` | `read` with history lines returns scrollback | `read_output` with `lines > 0` returns `unsupported` | true |
| `native_wait` | `wait` blocks inside the backend | informational (Shepherd would poll) | true |
| `survives_restart` | panes outlive a Shepherd restart | informational | false |

`RunSpec.profile` names the launch profile Shepherd resolved `argv` and `cwd` from,
and `RunSpec.profile_env` carries that profile's environment, already read from its
sources (a `Secret`, whose `Debug` is redacted). Only Shepherd sets them, only from a
profile.

`NativeBackend` (`src/native/`) is the only backend and is what Shepherd starts;
`OPENAB_SHEPHERD_BACKEND` accepts only unset, empty or `native`, and any other value
exits with status 2. The trait stays so that another backend can be added later
without touching the tools; none is planned. The herdr-core (hc) backend was removed.

### Native backend

The native backend runs each pane as a child of Shepherd on
its own PTY, with a `vt100` emulator for the screen. Nothing external is needed.
Pane ids are `n:<k>`
(internal; callers still use names).

- **Child environment.** `env_clear()`, then only `HOME`, `PATH` (default
  `/usr/bin:/bin:/usr/sbin:/sbin`), `USER`, `LANG` (default `en_US.UTF-8`) and
  `TERM=xterm-256color`, then the pane's [launch profile](#launch-profiles) `env`.
  No credential passes by name: `KIRO_API_KEY` and every other secret reach a child
  only through its profile (kiro-cli without a stored login authenticates with
  `KIRO_API_KEY`, so a Kiro profile sets it, for example from the keychain). A
  caller-chosen command (only with `allow_arbitrary_command`) gets instead its own
  `env` from `start_pane`: at most 32 variables, names `[A-Z_][A-Z0-9_]*`, values
  under the input rules (empty allowed), for that pane only. It may not set `HOME`,
  `PATH`, `USER`, `LANG`, `TERM` or any `DYLD_*`/`LD_*` name (refused with
  `invalid_input`). Shepherd's token, token file path, profiles path,
  `OPENAB_SHEPHERD_BACKEND` and everything else never reach a child. Shepherd's own descriptors are close-on-exec, native spawns are serialized, and the child marks every descriptor above 2 close-on-exec just before exec, so no descriptor above 2 survives exec, including ones Shepherd inherited from its parent. The list of descriptors is read in Shepherd (from `/proc/self/fd`, or `/dev/fd` on macOS) while the spawn lock is held, and the child only applies it; on Linux 5.11 or later `close_range` does the whole job in one call. If the list cannot be read, Shepherd scans every number up to the hard `RLIMIT_NOFILE` instead, and if that range is too large to scan (above 1048576) the spawn fails rather than leave descriptors open. A descriptor that cannot be marked also fails the spawn. One limit remains: a descriptor that another Shepherd thread opens without close-on-exec between the listing and the fork is not covered, so every descriptor Shepherd opens must stay close-on-exec. Cost: the listing is proportional to the number of open descriptors, not to the descriptor limit, so a spawn does not slow down when the limit is raised.
- **Process groups and descendants.** Each child is a session and group leader with
  the PTY as its controlling terminal. Shepherd also tracks every descendant of the
  child, by pid and start time: by parent pid every 250 ms while the child runs and
  again before any stop, and by membership of the child's group and session. So a
  descendant that moves to its own group or session, or whose parent exits, is
  still known. Removing a pane (`remove_pane`) and Shepherd's exit on SIGINT,
  SIGTERM or SIGHUP send SIGHUP and SIGTERM to the group and the descendants, wait
  up to 2 s (for `remove_pane`, its `timeout`), then stop (SIGSTOP) the group and
  descendants until no new one appears and SIGKILL them all. When the program exits
  on its own, what it left behind gets the same treatment at once. The leader stays
  an unreaped zombie only during that stop, so its pid (the group and session id)
  cannot be reused while Shepherd signals by it; a descendant is signalled only
  while its pid still has the recorded start time. Known limit: a descendant that
  is re-parented and fully detaches (new session, no terminal) before the next
  look at the process table (about every 250 ms) is never seen and survives
  (see Known limits). If Shepherd is killed with SIGKILL, children get only the
  PTY hangup.
- **Status.** OSC 7501 reports from the program are parsed in the emulator's
  stream (any field order, unknown keys ignored, split reads handled). The turn
  advances when the program leaves `working` or enters a new dialog (ADR §6).
- **Terminal queries answered:** DA1, DA2, DSR, CPR, DECXCPR, kitty `CSI ? u`
  (per-screen flag stacks), OSC 10/11/12 colour queries and OSC 7501 `?`.
- **Conditional input and keys are atomic.** The status check and the write happen
  under the same pane-state lock, so a status change cannot slip in between.
  `answer_dialog`'s Enter carries the turn and kind guard, and no other client can
  attach to a native pane. The Enter has no
  residual race.
- **Keys** follow DECCKM and the kitty flags the program pushed (kiro-cli 3.1.0
  pushes flag 1, disambiguate).

| Bound | Default |
|---|---|
| Live panes | 8 |
| Retained finished panes | 32 |
| Screen | 120 rows × 200 cols |
| Scrollback | 2,000 lines, about 12.8 MB per pane at 200 cols |
| Read chunk | 16 KiB |
| Raw output ring | 256 KiB per pane (diagnostics and the probe only) |
| OSC body | 1,024 bytes |
| Status log | 64 entries |
| Pending replies | 4 KiB |
| State and kind words | 32 bytes |
| argv | 256 elements |
| Extra env (per run, plus code-set) | 32 entries |
| Write deadline | 5 s |
| Paste settle | 100 ms quiet, at most 1 s |
| Start wait | 2 s |
| Read sync wait | 150 ms |
| Kill grace | 2 s (`remove_pane` `timeout`: 0 to 60 s) |
| Interrupt default | 10 s |

Worst case: 8 live and 32 retained finished panes each keep an emulator with full
scrollback (about 12.8 MB) and a 256 KiB raw ring, so about 520 MB in total.

Verified against kiro-cli 3.1.0 by `examples/native_probe.rs` (see
[`MVP-REPORT.md`](../MVP-REPORT.md)). The design is recorded in
[`docs/adr/native-core.md`](adr/native-core.md).

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

`name` must also match Shepherd's naming rule: 1 to 32 characters of `a-z`, `0-9`,
`-`, `_`, `.`.

The cleaned value is what gets sent. For `argv`, that means each element is trimmed,
so a leading or trailing space inside an argument is removed.

## Output rules

`read_output` removes CSI sequences (`ESC [ ...`), OSC sequences (`ESC ] ...` ending
in BEL or ST, including OSC 7501 status reports), DCS, SOS, PM, and APC strings,
two-byte and nF escapes, and lone ESC bytes. An unterminated sequence at the end of the
text removes the rest of the text. Newlines and tabs are kept.

## Authentication and network

- Host header: checked first, as protection against DNS rebinding. It must name a
  loopback host (the literal IPv4 loopback address, the bracketed IPv6 loopback
  address, or `localhost`, with or without a port), or
  match an entry in `OPENAB_SHEPHERD_ALLOWED_HOSTS` (empty by default, see
  [Behind tailscale serve](#behind-tailscale-serve)). Any other value gets HTTP 403,
  with no token check.
- Bearer token: compared in constant time. A missing or wrong token gets HTTP 401.
- Only `POST /mcp` is served. `GET` gets 405. Notifications get 202.
- The listener binds the validated loopback address only. The server does not run
  `tailscale serve`, Funnel, or any other public listener.
- Request bodies are capped at 1 MiB, read before the token check. Request bodies
  are not logged.

### Behind tailscale serve

`tailscale serve` can put the loopback listener on the tailnet over HTTPS. It
forwards the client's Host header, for example
`mcp-host.example-tailnet.ts.net:8445`, which the Host check refuses unless the
operator lists it. The server still binds loopback only, and the bearer token is
still required on every request.

```sh
tailscale serve --bg --https=8445 http://<loopback-address>:8767

OPENAB_SHEPHERD_TOKEN_FILE=<token-file> \
OPENAB_SHEPHERD_ALLOWED_HOSTS=mcp-host.example-tailnet.ts.net:8445 \
./openab-shepherd --port 8767
```

Clients then use `https://mcp-host.example-tailnet.ts.net:8445/mcp`.

`OPENAB_SHEPHERD_ALLOWED_HOSTS` is a comma-separated list of `name` or `name:port`:

- Names compare exactly and case-insensitively. One trailing dot is dropped from both
  the entry and the Host header. There are no wildcards, no suffix matching, and no
  address ranges, so `evil-mcp-host.example-tailnet.ts.net` and
  `mcp-host.example-tailnet.ts.net.evil.com` are refused.
- An entry with a port matches only that port. An entry without a port matches the
  name on any port, or with no port. Prefer the form with a port.
- An empty item, a scheme (`https://`), a path, a wildcard, whitespace inside an
  entry, brackets, or a bad port make the server exit with status 2 at startup.
- The startup log lists the configured entries once.

Keep Funnel off for this port (`tailscale funnel` must not be enabled): Funnel
publishes the listener to the internet, and only the bearer token would stand
between it and the panes.

## Tests

On the remote build host:

```sh
cd <repo>/openab-shepherd
export PATH=$HOME/.cargo/bin:$PATH
cargo test
cargo build --release
```

The unit tests need no external daemon. The backend-neutral tool tests use an in-memory
`FakeBackend`, and the config tests run `git init` in a scratch directory, so `git` must be installed.
The native integration tests (`native::tests`) run real programs on a PTY, so the
build host needs `sh`, `cat`, `printf`, `env` and `ls`.

The operator end-to-end runbook, which drives real Devin and OpenCode panes through the MCP tools, is in [`docs/operator-e2e/`](operator-e2e/README.md).

## CI

`.github/workflows/ci.yml` runs on every pull request and on pushes to `main`. It uses no
secrets and only read access to the repository.

- `test` runs on `ubuntu-latest` and `macos-latest`: `cargo fmt --check`,
  `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --no-fail-fast`.
  It times out after 20 minutes.
- `docker-build` checks that the Dockerfile's `rust:<version>-bookworm` builder matches
  `rust-toolchain.toml`, builds the `Dockerfile` with its default build arguments (no
  Devin CLI), pushes nothing, and runs `openab-shepherd --help` in the image. It times out
  after 15 minutes.

To reproduce the `test` job, run these three commands in the crate directory on the build
host (see "Build offload"), never on the laptop:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --no-fail-fast
```

`rust-toolchain.toml` pins the Rust version for CI, the build host and the Docker build
stage, so a new stable release does not change results on its own. rustup on the build
host installs the pinned version on first use. To move to a newer Rust, change `channel`
in `rust-toolchain.toml` and the `FROM rust:<version>-bookworm` line in the `Dockerfile`
in the same commit, then fix any new clippy or rustfmt findings.

### Stress workflow (advisory)

`.github/workflows/stress.yml` runs the `native::tests::` module repeatedly while one
`yes > /dev/null` per core keeps the runner busy, to surface tests that fail only under CPU
load. It runs on `ubuntu-latest` and `macos-latest`, only on `workflow_dispatch` and a weekly
schedule (Wednesday 03:17 UTC), never on pull requests. It is advisory: not a required check,
no retries, no secrets, 40-minute timeout.

```sh
gh workflow run stress.yml -f loops=20            # both thread settings
gh workflow run stress.yml -f loops=50 -f threads=1
```

Each OS runs `cargo test --locked --lib native::tests::` `loops` times with
`--test-threads=1` and `loops` times with the default thread count. The job summary shows
runs and failed runs per thread setting, then a table of failing tests with the number of
failed runs for each. The same text is printed in the job log. A run that fails without
naming a test (build error, panic outside a test) appears as `<no test name parsed> (exit N)`.
The job fails if any run failed. The logic is in `.github/scripts/stress-native.sh`.

## Build offload

The laptop never runs `cargo` or `rustc`. Sync the crate to the build host with
`rsync -a --delete --exclude target/`, which keeps the remote build cache. Build and
test there, then copy `target/release/openab-shepherd` back. The real-Kiro probe is
built by `cargo build --release --all-targets` at
`target/release/examples/native_probe`; copy it back with `scp` and run it on the
machine that has kiro-cli (it takes only `--keep-dir` and opens no listener).

## Container image

`Dockerfile` builds a linux/amd64 or linux/arm64 image: a Rust build stage, then a Debian trixie-slim
runtime with kiro-cli, git and tini, running as uid 1000. kiro-cli is pinned to
3.1.0 (`KIRO_CLI_VERSION`) and its download is checked against a SHA-256 for amd64
and for arm64. The image holds no token and
no profiles file. Build it on the remote build host, not the laptop:

```sh
docker build --platform linux/amd64 -t <registry>/openab-shepherd:<version> .
docker image ls <registry>/openab-shepherd:<version>
```

The default command serves on the IPv4 loopback address, port 8765, inside the
container. Nothing is
published, so reach it from a container that shares the network namespace (a tailnet
sidecar, for example). The token file and profiles file must be mounted or copied in
at runtime, owned by uid 1000, with the modes listed above:

```sh
docker run --rm --platform linux/amd64 \
  -v <token-dir>:/shepherd-conf:ro \
  -e OPENAB_SHEPHERD_TOKEN_FILE=/shepherd-conf/<token-file> \
  -e OPENAB_SHEPHERD_PROFILES=/shepherd-conf/<profiles-file> \
  --network container:<sidecar-container> \
  <registry>/openab-shepherd:<version>
```

To print the usage text without starting a server, run the binary by name:
`docker run --rm --platform linux/amd64 <registry>/openab-shepherd:<version> openab-shepherd --help`.

### Release

A release is a git tag `vX.Y.Z` on the merge commit of a release PR. Pushing the tag runs
`.github/workflows/release.yml`, which publishes the multi-arch image and creates the
GitHub release.

1. Open a release PR titled `chore(release): X.Y.Z` that bumps `version` in `Cargo.toml`
   and the package entry in `Cargo.lock`, and adds a `## X.Y.Z` section to `CHANGELOG.md`.
2. Merge it (squash).
3. Push an annotated tag on the merge commit:
   ```sh
   git fetch origin
   git tag -a vX.Y.Z -m "openab-shepherd vX.Y.Z" <merge-commit>
   git push origin vX.Y.Z
   ```
4. Watch the run (`gh run watch`) and verify:
   ```sh
   docker buildx imagetools inspect ghcr.io/openabdev/openab-shepherd:vX.Y.Z   # amd64 and arm64
   docker run --rm ghcr.io/openabdev/openab-shepherd:vX.Y.Z openab-shepherd --help
   gh release view vX.Y.Z --repo openabdev/openab-shepherd
   ```

The workflow runs only for a pushed tag matching `v[0-9]+.[0-9]+.[0-9]+`:

- `verify` checks that `Cargo.toml` has version X.Y.Z and `CHANGELOG.md` has a `## X.Y.Z`
  section (`.github/scripts/release-check.sh`), then runs `cargo fmt --check`, clippy and
  the tests.
- `image` builds `linux/amd64` and `linux/arm64` (QEMU) with default build arguments, so
  the image has no Devin CLI, and pushes `ghcr.io/openabdev/openab-shepherd:vX.Y.Z` and
  `:sha-<7 chars>`. It fails first if `:vX.Y.Z` already exists in the registry. It never
  pushes `latest`.
- `release` creates the GitHub release with the `## X.Y.Z` section as notes
  (`.github/scripts/release-notes.sh`; it fails on a missing or empty section). If a release
  for the tag exists already, it reports that and succeeds.

Permissions: the workflow has none by default. `verify` has `contents: read`, `image` has
`contents: read` and `packages: write`, `release` has `contents: write`. The only secret is
the automatic `GITHUB_TOKEN`.

**Security.** Anyone with write access to the repository can push a matching tag and so
publish an image under this name and create a release. Restrict who can push tags
(Settings, Rules, tag ruleset for `v*`) if that is too wide. The workflow does not use
`pull_request_target` and builds only the tagged commit.

**Dry run.** `workflow_dispatch` never publishes, whatever its inputs: it does not log in
to a registry, push or create a release. It runs `verify` for the given version, builds
amd64 (loaded locally, runs `openab-shepherd --help`) and arm64 (built, discarded):

```sh
gh workflow run release.yml -f dry_run=true -f version=0.3.0
```

Use a version that matches `Cargo.toml` on the ref you run it on. GitHub only accepts a
dispatch for a workflow file that exists on the default branch.

**GHCR package access (setting a maintainer must change).** The package
`ghcr.io/openabdev/openab-shepherd` was created by manual pushes. `GITHUB_TOKEN` can push to
it only if the package is linked to this repository or lists this repository under Package
settings, "Manage Actions access", with the Write role. When this workflow was added, the
packages API showed no `repository` on the package, so it is not linked. Until a maintainer
adds the repository there (https://github.com/orgs/openabdev/packages/container/openab-shepherd/settings),
the first real tag run fails at the push step, before any tag exists in the registry.

**Manual fallback** (used for 0.2.0 and 0.3.0, and valid if the workflow cannot publish).
After steps 1 and 2, push the annotated tag, create the release with
`gh release create vX.Y.Z --verify-tag --title vX.Y.Z --notes-file notes.md`, then build
each architecture on a host of that architecture (or emulated) and join them:

```sh
docker build --platform linux/arm64 -t ghcr.io/openabdev/openab-shepherd:vX.Y.Z-arm64 . && docker push ghcr.io/openabdev/openab-shepherd:vX.Y.Z-arm64
docker build --platform linux/amd64 -t ghcr.io/openabdev/openab-shepherd:vX.Y.Z-amd64 . && docker push ghcr.io/openabdev/openab-shepherd:vX.Y.Z-amd64
docker buildx imagetools create -t ghcr.io/openabdev/openab-shepherd:vX.Y.Z \
  ghcr.io/openabdev/openab-shepherd:vX.Y.Z-amd64 ghcr.io/openabdev/openab-shepherd:vX.Y.Z-arm64
```

Do not create `latest`, and never move or overwrite an existing version tag. If the
workflow was started for a tag, do not also publish by hand.

### Optional: Devin CLI (`INSTALL_DEVIN`)

The image can include the Devin CLI, but only when built with
`--build-arg INSTALL_DEVIN=1`. The default is `0`, so the published image has no
`devin` binary. Reason: the published GHCR image is public, and the Devin CLI terms for
redistribution and unattended server use have not been verified. A private or
operator-built image can opt in:

```sh
docker build --build-arg INSTALL_DEVIN=1 -t <registry>/openab-shepherd:<version> .
```

The Dockerfile pins Devin CLI 3000.12.57 for x86_64 and aarch64 Linux
(`DEVIN_CLI_VERSION`, `DEVIN_SHA256_AMD64`, `DEVIN_SHA256_ARM64`, from
`https://static.devin.ai/cli/3000.12.57/manifest.json`). The build fails on a checksum
mismatch. The checksum comes from the same host as the tarball, so it guards against
corruption, not tampering. The tarball is extracted to `/opt/devin`, and
`/usr/local/bin/devin` is a small wrapper that defaults `XDG_DATA_HOME` to
`/home/shepherd/.local/share` (writable) and runs the binary.

The image holds no Devin credentials. Provide them at run time as described in
[credentials.md](credentials.md): one interactive operator login, `credentials.toml`
kept as a Kubernetes Secret, copied to a writable path with mode 600.

Not verified: the Devin CLI terms of use; mounting the Secret in a pod; disabling
auto-update in the image (no config key was verified, so none is set; the CLI may
try to update itself at run time).

## Known limits (MVP scope)

- **No multi-client authorization.** Any holder of the token has full control of every
  pane.
- **Permission dialogs: text parsing, two layouts.** `answer_dialog` parses the dialog
  from screen text, for the pane's agent. Kiro is tested only against kiro-cli 3.1.0
  (V3 engine) shell permission dialogs, using the screen captured by the probe. Devin
  is tested against the layouts measured on Devin CLI 3000.12.43 and 3000.12.57 for shell commands.
  Other tools (fs_write, MCP), other programs, other versions, and `kind` values
  `question` or `auth` are untested or refused. `force` is never sent.
- **Native: no restart survival.** Native panes are Shepherd's children; they end
  when Shepherd exits.
- **Native: no viewer or attach.** Nobody can watch or type into a native pane
  except through the tools.
- **Native: 8 live panes.** `start_pane` is refused while 8 panes run. Free a slot
  with `remove_pane`; `list_panes` reports `live_panes` and `live_pane_limit`.
  Finished panes do not count against the 8 but keep their memory (up to 32 are
  retained, oldest evicted first) until removed.
- **Native: RIS does not reset kitty flags**, and mode 1047 (alternate screen
  without cursor save) is not emulated.
- **Native: a fast daemon escapes.** A descendant that is re-parented and fully
  detaches (calls `setsid`, so it has no group, session or terminal link to the
  pane, and its parents exit) before the next look at the process table, about
  every 250 ms, is never seen. It survives `remove_pane`, the program's own exit,
  and Shepherd's shutdown. One that only changes process group, or stays under a
  live parent until the next look, is ended (issue #10). macOS offers no
  unprivileged way to close this: kqueue `NOTE_TRACK` answers `ENOTSUP`, only root
  can create a coalition, and there is no subreaper or cgroup (measured on macOS
  15.7). A Linux subreaper is a follow-up.
- **A profile's credential goes to that profile's program and what it runs.** A Kiro
  profile with `KIRO_API_KEY` gives the key to kiro-cli, and Kiro's tools inherit it,
  so a caller who can prompt that Kiro can try to make it reveal the key. Profiles
  limit which program gets a credential and where it runs; they do not make the
  program keep it. With `allow_arbitrary_command`, a caller can still run anything as
  Shepherd's user, including reading files that user can read.
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
- **Grok Bot tested by hand only.** Grok Bot has connected to a deployed endpoint and
  called these tools in manual sessions. No automated test covers that client.
- **Stateless HTTP.** No SSE stream and no session ids. Clients that expect server
  push or sessions will not get them.
- **Token file check-then-read.** The mode and location are checked, then the file
  is read. Someone who can replace files in the token's folder could swap the file
  between the two steps. The 0600 mode and a folder outside the repository are the
  defense.
- **One backend.** The native core is the only backend and was verified against
  kiro-cli 3.1.0. openab-pty is not a backend.
- **Integration.** The native backend's real-program tests run in `cargo test` and
  the real-Kiro probe ran on the laptop ([`MVP-REPORT.md`](../MVP-REPORT.md), a dated record).
