# OpenCode CLI notes

This page holds the OpenCode notes that used to be in the [README](../README.md). The tool and error reference is in [operations.md](operations.md).

Observed on a developer machine with OpenCode 2.2.7 (`opencode --version`), installed with bun.
The startup, the permission dialog and the OSC 7501 states were measured in live sessions
with real model turns (the dialog on OpenCode v2.2.7); Shepherd itself has not yet been
checked end to end against OpenCode. Those checks are operator-run.

- Launch with `argv: ["opencode", "--standalone"]`. Without `--standalone`, OpenCode
  starts a background server (`opencode serve --service`) that keeps running after the
  TUI exits. Stop it with `opencode service stop`, which exits 0 and ends the process
  (observed on a developer machine).
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
  observed on a developer machine. The output filter removes it. Whether the native backend turns it
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
