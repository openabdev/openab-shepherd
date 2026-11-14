# Changelog

## Unreleased

### Changed

- Kiro permission dialogs: the tool name before the arrow must now be `shell`, `fs_write`, `mcp`
  or `web_fetch` (the set that was accepted by accident before); any other name is refused with a
  reason naming it. A command that wraps onto continuation lines is refused with a "wrapped
  command" reason instead of an options mismatch. The accepted set is unchanged. `answer_dialog`
  returns the Kiro tool name as `tool`. Measured on kiro-cli 3.3.0 (fixtures in
  `tests/fixtures/kiro-3.3.0/`). Refs #7.

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
