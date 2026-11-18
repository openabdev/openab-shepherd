# Dialog drift canary: operator runbook

For a person at a terminal on a **macOS host** or a **Linux host**. `dialog-canary.sh` answers one
question for one agent: does the permission dialog of the agent CLI installed on this machine still
parse with the rule this Shepherd ships? It is the manual check of issue #65. There is no scheduled
CI job for it, on purpose: it needs the agent's credentials and, for Devin and OpenCode, an
interactive login, and a failure is safe anyway (the parser refuses a dialog it does not know).
Run it before you move the pinned CLI version, or after an agent updates itself.

**Agents never run it**: it starts the real `openab-shepherd hold`. Issue #65 stays open until the
maintainer has run it against Kiro and pasted the result (see [Results](#results-for-the-issue)).

## What it does

For `kiro`, `devin` or `opencode` (one per run):

1. Checks that the agent can authenticate, **without ever logging in**. If it cannot, the script
   prints a `SKIP` line (counted in the summary), starts nothing and sends no prompt.
2. Compares `<cli> --version` with the version pinned in the repository's `Dockerfile`
   (`KIRO_CLI_VERSION`, `DEVIN_CLI_VERSION`; OpenCode has no pin, and its parser was measured on
   2.2.7). A different version is a notice, not a failure: the result then describes the new version.
3. Starts a loopback Shepherd on port 8769 with a throwaway token, an audit file (the server requires one with a clients file), a clients file that grants only
   `observe`, `read`, `control`, `start`, `dialog` and `remove`, and one launch profile for the agent.
4. Starts the pane. If the first screen asks for a login, the pane is removed and the check is a
   `SKIP`; nothing is typed.
5. Sends **one** prompt that asks the agent to run `touch canary_marker.txt` through its shell tool,
   and waits (at most 150 s) for `blocked` with kind `permission`.
6. Reads the screen and compares its **structure** with the recorded dialog: the title or heading, the
   tool and its command line, the options in order. Account names, paths, banners and the model's own
   wording differ from run to run, so it never compares bytes. The check is
   [`dialog-canary-check.py`](dialog-canary-check.py). For Kiro and OpenCode the reference is the
   committed fixture `tests/fixtures/replay/<agent>/03-permission-dialog.screen.txt`; Devin has no
   fixture file, so the layout `src/devin_dialog.rs` defines is the reference.
7. Calls `answer_dialog deny_once` and requires success. This goes through the production parser, so
   the script and the shipped rule cannot disagree about what a dialog is. (For OpenCode the dialect
   presses Esc; the tool does that.) It then requires that the marker file **does not exist**.
8. Always calls `remove_pane`, stops the holder and deletes its throwaway directories.

On a failure it prints a **drift report**: the installed version, the pinned version and the last 30
lines of the screen (`seen`). Read it before pasting it anywhere: it is a real screen.

## Cost and what each agent needs

| Agent | Model prompts | What you need |
|-------|---------------|---------------|
| `kiro` | 1 | `KIRO_API_KEY` set in the shell you run it from. The script checks that it is set (the value is never printed or put on a command line) and that `kiro-cli whoami` accepts it in a throwaway `HOME`. The profile hands it to the pane by `env:KIRO_API_KEY`. No browser login is needed. |
| `devin` | 1 | Your own login (`devin auth status` says `Logged in`) and a directory Devin already trusts: do README step D0 once for a directory and pass it as `CANARY_DEVIN_CWD`. Without it Devin asks to trust the throwaway directory and the script reports a `SKIP`. The default image does not contain Devin. |
| `opencode` | 1 | Your own provider login and a default model (`opencode auth list` shows a provider). The script writes an `opencode.jsonc` with ask rules for shell and edit into its throwaway directory. |

The agent sometimes adds a follow-up command of its own; the script never answers it, and it
removes the pane at the end.

## Before you start

1. A release binary built on a build host and copied to `~/.local/bin/openab-shepherd` (or set
   `SHEPHERD_BIN`), as in [README.md](README.md#0-preconditions-no-login-no-model-call).
2. `bash`, `python3`, `openssl`, `curl` and `pgrep`, and the agent CLI on `PATH` as an absolute path.
3. Run from this directory (`docs/operator-e2e` in a clone of the repository).
4. Check the script without starting anything: `bash -n dialog-canary.sh`, and
   `shellcheck -x dialog-canary.sh` where it is installed.

## Run it

```sh
cd docs/operator-e2e
export KIRO_API_KEY=...        # from your own secret store; kiro only
bash dialog-canary.sh kiro     2>&1 | tee /tmp/dialog-canary-kiro.out
bash dialog-canary.sh opencode 2>&1 | tee /tmp/dialog-canary-opencode.out
CANARY_DEVIN_CWD=/path/to/a/directory/devin/trusts \
  bash dialog-canary.sh devin  2>&1 | tee /tmp/dialog-canary-devin.out
```

Set the key in a shell whose history you control. The script never prints it and passes it only to
the Kiro pane.

Settings (environment): `SHEPHERD_BIN`, `PORT` (default 8769), `R_HOME` and `R_WORK` (both must
contain `shepherd-e2e-restart`), `KEEP=1` (leave the holder and the files behind),
`CANARY_DEVIN_CWD`.

## Reading the result

```
summary: 8 passed, 0 failed, 0 skipped
RESULT: PASS
```

| Line | Meaning |
|------|---------|
| `SKIP ...` | A prerequisite is missing (key, login, trust). Nothing was checked and no prompt was sent. It is not a failure and not a pass: the summary counts it. |
| `RESULT: INCOMPLETE` (exit 3) | Something was skipped, or nothing passed. It is not a pass. Only a run with `passed` above 0, no `SKIP` and `RESULT: PASS` is evidence for #65. |
| `NOTICE the installed ... differs from the pin` | The CLI is not the pinned version. The result is about the new version. |
| `FAIL the dialog has the recorded structure` | The title, tool line or options differ from the recorded dialog. The drift report shows the screen. Update the parser and the fixture together. |
| `FAIL answer_dialog deny_once succeeds` | The shipped parser refused the live dialog: the drift this script exists to find. The drift report shows `seen`. |
| `FAIL the marker file does not exist after deny_once` | The command ran although it was denied. Treat this as a security bug and report it. |

When the CLI version changed and the check passes, nothing needs doing. When it fails, record the new
screen with `record-replay-fixture.sh`, fix the dialect module, and run the script again.

## Self-check without a server

`cargo test --test dialog_canary` runs the structural check on the committed fixtures: it accepts
each agent's recorded dialog, rejects a copy with the title suffix changed or an option removed, and
requires the real parser to reach the same verdict on every screen. `cargo test --test
operator_e2e_docs` checks that the script parses and that this runbook and the README list it.

## Results for the issue

Paste the output of the Kiro run, which must show `RESULT: PASS` with no `SKIP` line (the `summary` and `RESULT` lines and the version line) into #65.
Issue #65 closes after that real run.
