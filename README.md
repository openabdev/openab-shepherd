# OpenAB Shepherd

## Goal

Shepherd is a bearer-protected MCP server that lets an MCP client, such as Grok Bot,
start and drive coding CLIs like Kiro in terminal panes. The client starts a pane,
types a prompt, waits for the turn to finish and reads the screen, all through MCP
tools. The safety rules are enforced by Shepherd, not by the agent: which programs
may start, where they run, which credentials they get, when typing is refused, and
which answers a permission dialog may receive.

## Features

- **Loopback-only server with bearer auth.** `--host` accepts only the literal IPv4
  or IPv6 loopback address, and IPv4 loopback is the default. Every request needs `Authorization: Bearer <token>`, compared in constant
  time. The Host header is checked first, against loopback names and an optional
  allowlist (`OPENAB_SHEPHERD_ALLOWED_HOSTS`) for use behind `tailscale serve`.
- **Pane lifecycle tools.** Nine MCP tools: `start_pane`, `send_input`,
  `read_output`, `get_status`, `wait_status`, `list_panes`, `interrupt_pane`,
  `answer_dialog` and `remove_pane`. Panes are addressed by name.
- **Blocked-dialog handling.** `send_input` refuses to type while a pane is blocked
  on a dialog and returns `pane_blocked`. `answer_dialog` accepts only `allow_once`
  or `deny_once`; the rule-creating options cannot be chosen. It reads the dialog
  for the pane's agent (`kiro`, `devin`, or `opencode`), re-checks the dialog, its kind and its
  turn number before it presses Enter. A Devin dialog never selects `Bypass mode`.
- **Launch profiles.** `start_pane` starts only what the operator's profiles file
  allows: a fixed `argv`, a list of allowed working directories, and environment
  variables. Each variable is a literal value or is read when the pane starts from
  a `keychain:` (macOS Keychain) or `env:` source. A profile names its `agent`
  (`kiro`, `devin`, or `opencode`) and a `permission_mode` (`normal`, `accept-edits`, or
  `bypass`). Caller-chosen commands are off
  unless the file sets `"allow_arbitrary_command": true`, and such panes never get
  a profile's environment. Without a profiles file, `start_pane` starts nothing.
- **Watchdog (optional, per profile).** A profile's `watchdog` section makes
  Shepherd check its panes every 5 s and type the profile's `nudge_text` —
  through the same path and refusal rules as `send_input` — when a pane's
  finished turn sits untouched for `idle_timeout_secs` and, when the profile
  sets `todo_file`, that file still has an unchecked item. At most
  `max_consecutive_nudges` nudges go in without progress the watchdog did
  not itself cause (the turn a nudge produces does not reset the count),
  and a blocked pane is never nudged. Every nudge and every give-up is
  logged to stderr. Off unless a profile sets it.
- **Native terminal core.** The default backend runs each pane on its own PTY with a
  `vt100` emulator, a cleared child environment and at most 8 live panes. Programs
  report their state (`idle`, `working`, `done`, `blocked`, ...) with OSC 7501
  escape sequences, which Shepherd parses into a status and a turn counter. It is
  the only backend. `OPENAB_SHEPHERD_BACKEND` accepts only unset, empty or `native`;
  any other value exits with status 2.
- **Process-tree cleanup.** `remove_pane`, a program's own exit and Shepherd's
  shutdown signal the pane's process group and every descendant Shepherd has seen,
  then kill what is left. Descendants are found by looking at the process table
  about every 250 ms, so a process that fully detaches faster than that is missed
  (see [Status and known limits](#status-and-known-limits)).
- **Container image.** The `Dockerfile` builds a Debian image that runs as uid 1000
  under tini, with kiro-cli pinned to 3.1.0 and checked against a SHA-256. The image
  holds no token and no profiles file.

## How it works

![Shepherd architecture](docs/architecture.svg)

1. The MCP client sends JSON-RPC over HTTP `POST /mcp` to the loopback listener,
   directly or through `tailscale serve`. Shepherd checks the Host header, then the
   bearer token.
2. The tool layer validates the arguments it recognizes (unrecognized keys are
   ignored, not rejected), then applies the safety rules: launch profiles, the
   per-pane lock, refusing input to a blocked pane, and the dialog checks. It reaches panes only through the `TerminalBackend` trait.
3. `start_pane` resolves a launch profile, reads its credentials, and the native
   backend spawns the program on a PTY with only `HOME`, `PATH`, `USER`, `LANG`,
   `TERM` and the profile's variables.
4. The program's output feeds the terminal emulator, which draws the screen and
   passes OSC 7501 status reports to Shepherd. `wait_status` returns when a turn
   ends (`done`), the pane blocks on a dialog (`blocked`), the program exits, or
   the wait times out.
5. When a pane is blocked on a permission dialog, the client reads the screen with
   `read_output` and answers with `answer_dialog`. When the work is done,
   `remove_pane` stops the program and its descendants and frees the slot.

## Quick start

Build on a build host with a Rust toolchain:

```sh
cargo build --release
```

Create a token file and a launch profiles file, both outside any git repository:

```sh
umask 077
openssl rand -hex 32 > <token-file>
chmod 600 <token-file>
```

```json
{
  "allow_arbitrary_command": false,
  "profiles": [
    {
      "name": "kiro",
      "argv": ["kiro-cli", "chat"],
      "cwds": ["<work-dir>"],
      "env": { "KIRO_API_KEY": "keychain:<keychain-item>" }
    }
  ]
}
```

On Linux, use `"env:<NAME>"` instead of `keychain:`; Keychain sources are macOS only.
Start the server:

```sh
OPENAB_SHEPHERD_TOKEN_FILE=<token-file> \
OPENAB_SHEPHERD_PROFILES=<profiles-file> \
./target/release/openab-shepherd --port 8765
```

Point the MCP client at `http://<loopback-address>:8765/mcp` with the header
`Authorization: Bearer <token>`. To reach it from another machine on the tailnet,
put it behind `tailscale serve` and allow that name, for example
`OPENAB_SHEPHERD_ALLOWED_HOSTS=<mcp-host>.example-tailnet.ts.net:8445`.

Every variable, profile field, tool argument, error code and bound is described in
[docs/operations.md](docs/operations.md).

How the operator signs in to the coding CLI or supplies its key is described in
[docs/credentials.md](docs/credentials.md).

## OpenCode CLI

Observed on a laptop with OpenCode 2.2.7 (`opencode --version`), installed locally with bun.
The startup, the permission dialog and the OSC 7501 states were measured in live sessions
with real model turns (the dialog on OpenCode v2.2.7); Shepherd itself has not yet been
checked end to end against OpenCode. Those checks are operator-run.

- Launch with `argv: ["opencode", "--standalone"]`. Without `--standalone`, OpenCode
  starts a background server (`opencode serve --service`) that keeps running after the
  TUI exits. Stop it with `opencode service stop`, which exits 0 and ends the process
  (observed on the laptop).
- Never pass `--auto`, and never set `session.permissions` to `autoaccept`. Both
  approve permission prompts without a dialog. A profile whose `argv` or `extra_args`
  contains `--auto`, a `--auto-…` flag, or an `autoaccept` or `auto-accept` spelling is
  refused when the config loads. This applies when the profile sets `agent: "opencode"`,
  or when any argv element names `opencode` (for example `npx opencode`, `sh -c "opencode …"`,
  or `/usr/local/bin/opencode`), even with no `agent` field. The check does not cover `env` or OpenCode's own config
  file, and it does not cover a command given to `start_pane` through
  `allow_arbitrary_command`.
  This check prevents accidental auto-approve configuration. It matches by name (case-insensitive substring `opencode`) and does not defend against a deliberately renamed binary, a wrapper script, or the `allow_arbitrary_command` path.
- At startup OpenCode reports `state=idle:app=opencode` over OSC 7501. This report was
  observed on the laptop. The output filter removes it. Whether the native backend turns it
  into a `get_status` result is not yet verified. It is an open point pending a live check.
- A launch profile may name `agent: "opencode"`, but only with `permission_mode` `normal`:
  no OpenCode mode was measured, so `accept-edits` and `bypass` are rejected.
- The permission dialog was measured on OpenCode v2.2.7 in a live session (shell and file
  edit). OpenCode reports `state=idle|working|blocked|clear` over OSC 7501, with ST
  terminators and colon-separated fields, and `kind=permission` on `blocked`. It has no `done`
  state: a turn ends `clear`, then `idle`. For an OpenCode pane, `wait_status` reports
  `done` for an `idle` that follows at least one turn, and `cancelled` after a dialog that
  `answer_dialog` denied.
- **A dialog appears only when the effective OpenCode config has an `ask` rule.** With no
  rule, a shell command ran at once and the pane went `working` to `idle` with no `blocked`.
  Shepherd then has no dialog to answer, and that turn is complete. To get a dialog, the
  operator must set `ask` rules for `shell` and `edit` in OpenCode's own config, for
  example in `~/.config/opencode/opencode.json`:
  `"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}, {"action": "edit", "resource": "*", "effect": "ask"}]`.
  No environment variable or explicit config path that sets permissions is documented
  (`opencode --help` and opencode.ai/docs/config were read), so Shepherd does not set
  one and writes no file into the working directory.
- `answer_dialog` on an OpenCode pane needs the pane to be `blocked` with kind `permission`
  and the screen to show the whole dialog: `△ Permission required`, a `$ <command>` or
  `→ Edit <path>` line, then `Allow once`, `Always allow`, `Reject` in that order and the
  `ctrl+f fullscreen` hint. Anything else is refused with no key sent.
  - `deny_once` sends `Esc`, which cancels exactly like `Reject` (measured with marker
    files) and ends the turn (`idle`). It re-checks status and turn just before the key.
  - `allow_once` presses `Enter` only after it verifies the highlight. OpenCode draws the
    highlight as a background colour (no reverse video, no glyph), and the colour depends on
    the terminal: a 256-colour palette index without `COLORTERM`, truecolor with it. The
    native backend therefore applies a relative rule to the styled screen cells (colours and
    the reverse attribute, used internally and never returned by a tool) and presses `Enter`
    in the same step, under the status, kind and turn guard. It presses only when the dialog
    text checks pass and `Allow once` has a background that differs from both other options,
    while those two have the same background as each other. Colours are compared as values
    (default, palette index or RGB) without converting between palette and RGB, so the rule
    works with palette or truecolor. All three equal, the highlight on another option, any
    other pattern, a label with mixed backgrounds, a default background on `Allow once`,
    reverse video on any label, text that changed before the key, or a backend that cannot
    read colours: refused with no key sent. Measured on OpenCode v2.2.7 only; the foreground
    is not checked, and a theme that highlights differently is unmeasured.
  - `Always allow` is never selected, and `Right` is never sent.
- Not measured: MCP tools (the configured server was failing), number or letter
  shortcuts and `Tab` in the dialog, the effect of `Always allow`, and a file write without
  an `ask` rule.

## Status and known limits

Shepherd is an MVP (version 0.2.0). The native backend was verified against
kiro-cli 3.1.0 with a real-Kiro probe; the results are in
[MVP-REPORT.md](MVP-REPORT.md) (a dated record, not a current status), and the native core's design is in
[docs/adr/native-core.md](docs/adr/native-core.md).

- **No multi-client authorization.** Anyone holding the token controls every pane.
- **Two dialog choices only.** `answer_dialog` answers `allow_once` or `deny_once`
  and nothing else. Other dialog kinds (`question`, `auth`) are not answered.
- **Text-based dialog parsing.** The dialog is parsed from screen text. Measured
  agents: kiro-cli 3.1.0 (V3 shell permission dialogs), Devin CLI 3000.12.43 and 3000.12.57
  (shell commands) and OpenCode v2.2.7 (shell and edit dialogs, see below). For OpenCode,
  `deny_once` is answered with Esc; `allow_once` presses Enter only when the styled
  screen cells show `Allow once` with a background that differs from the other two options, which match each other.
- **A fast detached descendant escapes cleanup.** A descendant that is re-parented
  and fully detaches (new session, no terminal, parents gone) within the 250 ms
  between looks at the process table is never seen and survives. macOS has no
  unprivileged way to close this gap; a Linux subreaper is a follow-up.
- **Panes do not survive a server restart.** Native panes are children of Shepherd
  and end with it.

The full list, including the output filter's limits,
is in [docs/operations.md](docs/operations.md#known-limits-mvp-scope).

## License

MIT. See [LICENSE](LICENSE).
