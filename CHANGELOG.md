# Changelog

## 0.3.0

### Changes

- A launch profile may set a `watchdog`: Shepherd checks its panes every 5 s
  and, when a finished turn sits untouched past `idle_timeout_secs` (with an
  unchecked item left in the profile's `todo_file`, when one is set), types the
  profile's `nudge_text` through the `send_input` path. At most
  `max_consecutive_nudges` nudges go in without progress the watchdog did
  not itself cause. Off unless a profile sets it (issue #38).

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
