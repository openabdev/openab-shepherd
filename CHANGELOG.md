# Changelog

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
