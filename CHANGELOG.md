# Changelog

## Unreleased

- Kiro permission dialogs: `fs_read` (a read outside the workspace, measured on kiro-cli 3.3.0)
  is now accepted, so `answer_dialog` can answer it. Web search and wrapped shell commands stay
  refused. Fixtures for the new-file write, web fetch and web search keys and the wrapped shell
  dialogs are added under `tests/fixtures/kiro-3.3.0/`.

### Added

- A task ledger module (`src/ledger/`, `docs/ledger.md`): dispatch states, an append-only fsynced event
  log with replay and crash recovery to `delivery_unknown`, and a pure matching rule for worker
  completion records. Self-contained and not wired into the server; no behavior changes.

## 0.5.0

Released 2026-11-17.

This release lets the server restart without losing panes, adds clients with scopes and an audit
log, lets agents create their own routines, lets a trusted client pass call-level flags, and adds
the backend half of sharing a pane with a person. Nothing that a 0.4.0 deployment does by default
stops working, with one exception: the container image now starts the pane holder (see below).

### Upgrading from 0.4.x

- **The container's default command changed.** The image now runs
  `openab-shepherd hold -- --host 127.0.0.1 --port 8765` under tini, where 0.4.0 ran
  `openab-shepherd --host 127.0.0.1 --port 8765`. The holder owns the panes, so `SIGUSR2` restarts
  the server without ending them. Stopping the container still ends the panes. To keep the old
  single-process behaviour, override the command with `openab-shepherd --host 127.0.0.1 --port 8765`.
  The entrypoint (`tini --`) is unchanged. (#90)
- **The `watchdog` profile key is now `routine`.** Rename the key. The old key still works as a
  deprecated alias and prints one warning to stderr per profile; a profile that sets both keys is a
  startup error. Limits and behaviour are unchanged. Anything that matches log or audit text must
  change: the stderr lines are `routine nudge` and `routine gave up` (were `watchdog ...`), and the
  audit actor is `system:routine` (was `system:watchdog`).
- **New error codes.** Clients that match on the `error` field should handle:
  `pane_replayed` (under `hold`, a pane whose output was longer than the 1 MiB tail refuses typed
  input until it repaints; the only new code a server run as before can return),
  `pane_human_controlled` and `pane_draft_pending` (a person holds the input lease or has an unsent
  draft; nothing can take a lease in this release),
  `routines_not_allowed` (`routine_*` on a pane whose profile has no `dynamic_routines`),
  `pane_needs_trust_acceptance` (see the Kiro trust-all warning below) and
  `audit_unavailable` (a mutating call when the audit file is set and cannot be written).
- **New scopes and tools.** `routine` allows the new tools `routine_create`, `routine_list`,
  `routine_update` and `routine_delete`. `start:args` allows `extra_args` on a profile start.
  `draft_clear` is a new tool (scope `lease:preempt`). `attach` and `attach:write` are reserved.
  No existing client gains any of them: a scope is held only if the clients file lists it, and the
  single token holds the same seven scopes as before (`observe`, `read`, `control`, `start`,
  `start:arbitrary`, `dialog`, `remove`). `start_pane` gains the optional properties `agent` and
  `extra_args`, so a client that pins the tool schemas will see a change.
- **A single-token deployment cannot use `extra_args`.** A server with only
  `OPENAB_SHEPHERD_TOKEN` or a token file and no clients file gets `forbidden` naming `start:args`.
  To use it: make a token and its SHA-256 hash, unset `OPENAB_SHEPHERD_TOKEN` and
  `OPENAB_SHEPHERD_TOKEN_FILE` (a clients file refuses them), set `OPENAB_SHEPHERD_CLIENTS` and
  `OPENAB_SHEPHERD_AUDIT_FILE`, write a clients file whose entry lists the old token's scopes plus
  `start:args` and a short `profiles` list, give the client the token, and restart. The full recipe
  is in `docs/operations.md` ("Call-level `extra_args` for a profile start").
- **No migration for the audit file or the clients file.** Neither existed in 0.4.0 or any earlier
  release, so there is no old shape to convert. The `legacy` client name and `allow_legacy_token`
  existed only on `main` between releases (a clients file that still sets `allow_legacy_token` is
  refused with a message to delete it).
- **Kiro `-a` (trust all tools).** Kiro shows a warning first: "Kiro is running in trust all tools
  mode", with `No, exit` as the default. Typed text is ignored and Enter ends the session.
  Shepherd never answers it. A `send_input` or a routine nudge to a Kiro pane that has not finished
  a turn and shows that warning is now refused with `pane_needs_trust_acceptance`, and nothing is
  typed. A person accepts the warning, or the operator sets `chat.disableTrustAllConfirmation` to
  `true` in the Kiro settings of the HOME the pane uses (measured on kiro-cli 3.3.0: choosing
  `Yes, and don't ask again` writes it to `$HOME/.kiro/settings/cli.json`, while `Yes, I accept`
  persists nothing). That setting removes the only human gate on the warning for every Kiro
  started with that HOME; it is the operator's decision, never Shepherd's. (#114, #124)
- **The image ships kiro-cli 3.3.0** (was 3.1.0), the version the dialog parser was measured on.
  Rebuilt images pick up the new CLI. (#109)
- **Pane children no longer get a descriptor sweep.** Shepherd still marks its own descriptors
  close-on-exec and serializes spawns, but it no longer shuts descriptors inherited from its
  parent before exec. Start Shepherd with a clean descriptor table (for example under a service manager or in a container). (#74)

### Added

- `docs/operator-e2e/dialog-canary.sh <kiro|devin|opencode>` (issue #65): an operator script that
  sends one prompt to the installed agent CLI, compares its permission dialog with the recorded
  one and answers `deny_once` through the shipped parser. It never logs in and skips when the
  agent cannot authenticate. No scheduled CI. See `docs/operator-e2e/dialog-canary.md`.
- Restart survival (issue #76; #83 to #86, #88 to #92, #94). `openab-shepherd hold -- <server flags>`
  runs a holder that owns the panes and starts the server as a child. `SIGUSR2` on the holder
  restarts the server and the panes keep running; a new server adopts them, replays their output
  silently, and rebuilds each pane's agent kind, `answered` flag and routine watch.
  `survives_restart` is true under `hold`. A pane whose output exceeded the holder's 1 MiB tail is
  refused typed input with `pane_replayed` until it repaints. The bare command is unchanged. See
  "Restart survival" in `docs/operations.md`. Operator scripts for the real-server scenarios are in
  `docs/operator-e2e/restart-survival.md`; `examples/record_tail.rs` records replay fixtures by hand.
- Clients, scopes and the audit log (issue #4; #68, #75, #80). `OPENAB_SHEPHERD_CLIENTS` names a
  clients file with one entry per caller (token hash, scopes, profiles, expiry), and
  `OPENAB_SHEPHERD_AUDIT_FILE` records a `call` and an `outcome` line per mutating call. Scopes:
  `observe`, `read`, `control`, `start`, `start:arbitrary`, `start:args`, `dialog`, `remove`,
  `attach`, `attach:write`, `lease:preempt` and `routine`. The trust model is written down in
  `docs/adr/trust-model.md` (#72), with a section on trusted clients.
- Agent-created routines (issue #98; #100, #102 to #105). Four tools, `routine_create`,
  `routine_list`, `routine_update` and `routine_delete`, let an agent or any client with the new
  `routine` scope ask Shepherd to type a fixed line into a pane every N seconds (trigger `every`,
  action `text`). A profile opts in with `dynamic_routines`; without it `routines_not_allowed` is
  returned. Routines are kept in the pane's meta, survive a restart under `hold`, skip busy or
  replayed ticks instead of queueing them, and are audited as `system:routine`. A pane's program
  sees its own name in `SHEPHERD_PANE`, and a routine nudge writes an outcome line. The single
  token holds no `routine` scope. See `docs/routine.md` and `docs/operator-e2e/routine-dynamic.md`.
- Call-level `extra_args` for a profile start (issue #113; #116, #117). A client with the new scope
  `start:args` can pass flags with `start_pane {profile, extra_args}`. They are appended after the
  profile's own arguments and cannot replace or remove one, change the program, the directories,
  the environment or the credentials. The call needs `start`, the profile on the client's list and
  `start:args`; `start` and `start:arbitrary` do not imply it. `null` and `[]` add nothing and need
  no scope. The agent's own refusals (OpenCode auto-approve flags, Devin `--permission-mode` and
  `--respect-workspace-trust`) apply to the extra flags, and every `invalid_input` from
  `extra_args` names the field (#122). The audit records `launch` `profile+args`, the count and the
  flag names, never a value. Flags run with the profile's credentials, so never put a secret in them.
- `agent` for `start_pane` with `argv` or `line` (issue #113; #115). A caller that may start its
  own command can declare `kiro`, `devin` or `opencode`, so `answer_dialog` and the agent guards
  apply to the pane. No new scope; the declaration is audited (`launch`, `argv0`, `agent`) and
  never checked against the program.
- Operator end-to-end scripts for the new features: `docs/operator-e2e/trusted-client.sh` (#118)
  and `docs/operator-e2e/routine-dynamic.sh` (#105, #106, #112), each with a runbook.
- Shared panes, the backend half only (issue #77; #95 to #97). The native backend has an input
  lease and a draft flag per pane. While a person holds the lease, or has typed and not pressed
  Enter, `send_input`, key presses, `answer_dialog`, `interrupt_pane`, `remove_pane` and the
  routine's nudge are refused with `pane_human_controlled` or `pane_draft_pending`, and are never
  queued. The draft is written to the pane's meta before the person's first byte and survives a
  restart under `hold`; a lease does not. Also: the `draft_clear` tool (scope `lease:preempt`), a
  `human` object in `get_status`, and resizing by the lease holder (10 to 500 columns, 5 to 300
  rows). No client can take a lease yet and there is no attach transport, so a server started as
  before behaves as before.

### Changed

- Container image: the default command is the pane holder (see "Upgrading from 0.4.x"), and the
  image ships kiro-cli 3.3.0 instead of 3.1.0. (#90, #109)
- The built-in watchdog is now the routine: profile key `routine`, `src/routine.rs`, stderr lines
  `routine nudge` / `routine gave up`, and audit actor `system:routine`. Behaviour and limits are
  unchanged.
- Kiro permission dialogs: the tool name before the arrow must now be `shell`, `fs_write`, `mcp`
  or `web_fetch` (the set that was accepted by accident before); any other name is refused with a
  reason naming it. A command that wraps onto continuation lines is refused with a "wrapped
  command" reason instead of an options mismatch. The accepted set is unchanged. `answer_dialog`
  returns the Kiro tool name as `tool`. Measured on kiro-cli 3.3.0 (fixtures in
  `tests/fixtures/kiro-3.3.0/`). (#70) Refs #7.
- Kiro trust-all warning: `send_input` and the routine nudge refuse with
  `pane_needs_trust_acceptance` only on a positive match: a Kiro pane at turn 0 or unknown, status
  `idle`, whose screen shows both the title phrase and the `No, exit` option line. A blank screen,
  a failed read or the phrase alone send as before. One screen read per send until the first turn,
  none afterwards. (#114, #124)
- A pane's `exited` event now waits for the pty to drain by progress: after the leader exits, the
  reaper waits until the reader hits end of file, or 500 ms have passed with nothing queued or in
  flight, or a 5 s cap. Normal exits are unchanged; a wedged output sink or a descendant that keeps
  printing can delay `exited` by up to 5 s (was 500 ms). This stops `exited` overtaking the end of
  a quick exit's output under load. (#125, issue #87)
- Pane children no longer get a descriptor sweep (see "Upgrading from 0.4.x"). (#74)
- Audit log: one `call` line before each mutating tool and a best-effort `outcome` line after it
  (and after `read_output`); `list_panes`, `get_status` and `wait_status` write nothing. Failed
  authentications go to stderr, not to the audit file. The 0600/0700 mode checks stay. (#75)
- Clients file: the single token is the client `default`. With a clients file, a token file or an
  env token is a startup error that names the fix. The 0600 mode check, the 1 MiB cap and the
  bearer-length cap stay. (#80)
- `sha2` 0.10 (no default features) is a new dependency, used for token hashes.
- Documentation: the README, the operations reference and the ADR statuses were brought in line
  with the current design, diagrams were added, and routine documentation moved to
  `docs/routine.md`. (#67, #72, #82, #107)

### Deprecated

- The `watchdog` profile key. Use `routine`. The alias prints one warning to stderr per profile that uses it.

### Fixed

- Flaky tests: the fd-count and front-pid checks in the restart tests, the hidden-flags test
  (now bounded) and the 1 MiB tail test (no longer goes through a front) were made deterministic
  (#121); `large_output_of_a_quick_exit_is_received_up_to_the_cap` was a real race in the exit
  drain (#125). No product behaviour changed except as listed under Changed.
- `trusted-client.sh` gave a false failure for a client that held both `start:arbitrary` and
  `start:args`; it now uses a client with `start:arbitrary` only. (#122)
- `routine-dynamic.sh` read counters too early and failed on macOS, where `RLIMIT_FSIZE` is not
  enforced; it now waits for the engine and skips that step on macOS. (#106, #112)

### Internal

- Tests: a snapshot of the tool names and input schemas (#71), replay fixtures for kiro and
  opencode (#94), tini and zombie handling in the image (#90, #91), and the real Kiro trust-all
  warning screen as a fixture (#124).
- CI: the test step is split into build and test steps with step timeouts, a 60 s heartbeat, and
  a diagnostic dump after 300 s without progress; it does not fix the macOS hang tracked in #108.
  (#120)

## 0.4.0

### Breaking changes

None. The watchdog is off unless a launch profile sets it, and the new fields are optional.

### Added

- Built-in watchdog for stalled panes, contributed by ttmuse (issue #38, #40). A launch profile may
  set a `watchdog` section. Shepherd then checks that profile's panes every 5 s. When a pane's turn
  has finished and sits untouched past `idle_timeout_secs` (and, when the profile sets `todo_file`,
  that file still has an unchecked `- [ ]` item), it types the profile's `nudge_text` through the
  same path and refusal rules as `send_input`. At most `max_consecutive_nudges` nudges go in
  without progress the watchdog did not itself cause; `cooldown_secs` spaces them. It is off by
  default and watches only panes started from a profile that sets it. It never answers a permission
  dialog and never types into a blocked pane. Each nudge is sent with a guard
  (`if_state` and `expect_turn`), so a pane that changed state or started a new turn since the
  check is refused rather than typed into. A `nudge_text` that is a bare confirmation such as `y`,
  `yes` or `1` is refused when the profiles file is read. Every nudge and give-up is logged to
  stderr. See `docs/operations.md`. (#40)
- The same PR adds a bounded wait on the pane lock (a watched pane that another call holds for more
  than 2 s is skipped for that check) and a stop when a nudge was pasted but its Enter was refused,
  so the text is not pasted twice. (#40)

### Fixed

- A pane could lose the output of a program that exits immediately on macOS, such as
  `/usr/bin/env`: the first read from the PTY returned nothing. Shepherd now keeps its copy of the
  slave side open until the pane's leader exits. Linux was not affected. (#56, issue #54)
- Two test races: `native::tests::interrupt_sends_ctrl_c_to_a_program_without_status` (the Ctrl-C
  could reach a starting shell instead of the program) and the `http::tests` port race (a port was
  released and rebound, so a test could talk to another listener). No product behaviour changed.
  (#53, issues #46, #51)

### Internal

- Tests: the hangup test is renamed to say what it checks, a test covers `remove` with the default
  grace on a program that ignores HUP and TERM, a pure test covers the child environment
  whitelist, and several negative assertions that could pass on an empty screen now wait for a
  marker first. (#57, issue #52)
- CI: an advisory stress workflow runs the native tests under CPU load, weekly and on
  demand. It is advisory, not a required check. (#58)
- CI: pushing an annotated `vX.Y.Z` tag runs `release.yml`. It checks that `Cargo.toml` and
  `CHANGELOG.md` match the tag, runs fmt, clippy and tests, builds and pushes a multi-arch
  `ghcr.io/openabdev/openab-shepherd:vX.Y.Z` and `:sha-<7>` (never `latest`) and creates the GitHub
  release from this file. Runbook in `docs/operations.md`. (#59)
- The image carries `org.opencontainers.image.*` source labels, which link the GHCR package to this
  repository. (#60)

## 0.3.0

### Breaking changes

- The herdr-core (hc) backend is removed. Native is the only backend.
  `OPENAB_SHEPHERD_BACKEND` accepts only unset, empty or `native`; any other value,
  including `hc`, exits with status 2. `HC_SOCKET` is no longer read. (#31, #32)

### Added

- Per-agent dialect layer for permission dialogs, with the `agent` (`kiro`, `devin`) and
  `permission_mode` (`normal`, `accept-edits`, `bypass`) launch-profile fields. Devin CLI is
  supported: `answer_dialog` answers only when the highlighted row matches, Bypass mode is never
  answered with Allow, and the directory trust prompt is never answered. (#8)
- `send_input` refuses mode-switching slash commands such as `/yolo` on Devin panes. (#24)
- OpenCode CLI support: startup status is read, `deny_once` sends Esc, and `allow_once` is refused
  unless the highlighted option can be verified. Launch profiles for OpenCode must use
  `permission_mode` `normal`, and `--auto` and `autoaccept` spellings are refused. (#13, #29)
- OpenCode `allow_once` presses Enter only after the selected option is verified from the styled
  screen cells (new `TerminalBackend::keys_if_screen`); any doubt refuses without a key. (#33)
- Panes that are blocked on a login (`kind=auth`) report a clear not-logged-in message. (#23)
- The credential contract for the coding CLIs is documented in `docs/credentials.md`, linked from
  the README and `docs/operations.md`. (#18, #25, #26)
- The Dockerfile can install Devin CLI 3000.12.57 (amd64 and arm64, sha256 pinned) with
  `--build-arg INSTALL_DEVIN=1`. The default is `0`, so the public image has no Devin binary. (#27)
- CI: `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on Linux and macOS, plus a
  Docker build check that runs `openab-shepherd --help`. (#45)
- The operator end-to-end runbook and helper scripts for Devin and OpenCode are in
  `docs/operator-e2e/`. (#42)
- `docs/terminal-clients.md` describes how a terminal client such as OpenAB Connect uses Shepherd
  as the supervision plane beside openab-pty. (#41)

### Fixed

- Pane children receive only descriptors 0, 1 and 2: every inherited descriptor above 2 is marked
  close-on-exec before exec. (#47)
- The descriptor list is now computed in the parent rather than in the child between fork and
  exec. If the list cannot be built, the parent falls back to a bounded scan; a range too large to scan, or a descriptor that cannot be marked, fails the spawn closed.
  Tests for this are isolated from each other. (#49)
- `answer_dialog` accepts the Devin 3000.12.57 permission dialog layout. (#34)
- `deny_once` on Devin re-reads the highlight after each Down key, so it is no longer refused
  while the screen is still redrawing. (#37)
- OpenCode `allow_once` verifies the highlight with a palette-independent relative rule, so it
  works in panes that run with a cleared environment. (#35)
- An OpenCode pane that goes idle after `allow_once` reports `done`, not `cancelled`. (#36)
- Tests: the outside-git-repo profiles test no longer depends on the umask (#43), the Linux hangup
  test no longer exceeds the grace in debug builds (#44), and clippy
  `unnecessary_mut_passed` in `open_pty` is fixed (#19).

### Security

- Descriptors that Shepherd's parent left open without close-on-exec reached every pane child, so
  the agent's tools could read and write them. They are now closed on exec (#47, #49).
  `docs/operations.md` states the guarantee and its one residual: another thread opening a
  non-close-on-exec descriptor between the listing and the fork.

## 0.2.0

### Changes

- The native terminal backend is the default. `OPENAB_SHEPHERD_BACKEND=native` selects it. The hc backend is still selectable.
- The `remove_pane` tool is available. `list_panes` reports `live_pane_limit`.
- `start_pane` refuses to start without a launch profile unless `allow_arbitrary_command` is true.
- `OPENAB_SHEPHERD_ALLOWED_HOSTS` sets the Host header values Shepherd accepts, for use behind `tailscale serve`.
- `answer_dialog` accepts only `allow_once` and `deny_once`. It waits for the dialog to render before answering.
- Process-tree cleanup fix (commit d4ecf26).
- Kiro CLI 3.1.0 is pinned in the Dockerfile for linux/amd64. The ARM64 pin is also 3.1.0 (commit 3a94442).

### Breaking changes

- `start_pane` refuses to run without a launch profile.
- `list_panes` and `start_pane` return Shepherd's own result shape.

### Known limits

- No multi-client authorization.
- Dialog parsing is tested only on kiro-cli 3.1.0 V3 shell dialogs.
- Panes do not survive a server restart.
- Detached descendants that exit faster than the poll interval (about 250 ms) are not tracked on either OS.
