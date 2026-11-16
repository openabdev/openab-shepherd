# Agent-created routines: operator end-to-end runbook

For a person at a terminal on a **macOS host**; it also runs on Linux. `routine-dynamic.sh`
starts the real `openab-shepherd hold` and drives the four `routine_*` tools through MCP. **Agents
never run it**: an agent must not start the Shepherd server. Issue #98 stays open until you have run
it and pasted the result into the issue (see [Results](#results-for-the-issue)).

It calls no model. The panes are `sh -c` scripts that print OSC 7501 status reports and append
whatever is typed into them to `typed.txt`. The calls an agent would make are made by the script
with the agent's own token (client `agent-a`, scopes `routine` and `observe`), so the checks come
from MCP fields, files, process ids, the audit file and the holder log. Read
[Agent-created routines](../routine.md#agent-created-routines) first.

Everything is throwaway: tokens, a clients file, the audit file and a profiles file in
`~/.config/shepherd-e2e-restart-dynamic`, pane working directories and the holder log in
`/tmp/shepherd-e2e-restart-dynamic`, and loopback port 8767 (`PORT=<n>` changes it; it differs from
the other runbooks' 8765 and 8766). The script stops its holder, which ends its panes, and removes
these files when it ends (`KEEP=1` keeps them).

## Before you start

1. A release binary built on the build host, copied to
   `~/.local/bin/openab-shepherd` (or set `SHEPHERD_BIN`). Note the commit you built from.
2. `bash`, `python3`, `openssl`, `curl` and `pgrep` on the machine.
3. Run from this directory (`docs/operator-e2e` in a clone of the repository).
4. Check the script without starting anything: `bash -n routine-dynamic.sh`, and
   `shellcheck routine-dynamic.sh` where it is installed.

## Run it

```sh
cd docs/operator-e2e
bash routine-dynamic.sh 2>&1 | tee /tmp/routine-dynamic.out
```

It takes about 12 minutes, five of them with the front down. `DOWN_SECS=120` shortens that (the
check that nothing is made up still holds with two intervals missed); the full run uses 300.

## What it sets up

- A profile `dyn` with `"dynamic_routines": {"min_every_secs": 60, "max_expires_secs": 3600}` and a
  profile `plain` without the key. Three `dyn` panes (`dyn`, `busy`, `exp`) and one `plain` pane.
- A clients file with `ops` (scopes `observe`, `read`, `control`, `start`, `remove`; starts the
  panes; has no `routine` scope) and `agent-a` (scopes `routine` and `observe`; no `control`).
- An audit file in a directory of mode 700.
- The holder starts with a file-size limit and `SIGXFSZ` ignored, for step 9.

## The ten checks

The numbers are those of issue #98's operator list. Step 10 runs after step 6, because the routine
it needs has expired by then.

| # | What happens | What proves it |
|---|--------------|----------------|
| 1 | Holder, clients file, audit file and a profile with `dynamic_routines` start | MCP answers under the `ops` token |
| 2 | Panes start; each pane's `SHEPHERD_PANE` is its name | `pane-env.txt` in each pane's directory |
| 3 | `agent-a` calls `routine_create` (every 60 s, with a `todo_file`); `routine_list` shows `next_due_in_secs`. Refusals: interval below the floor, bare `yes`, `todo_file` with `..`, an `idle` trigger (`routine_invalid`, with `field`), a `plain` pane (`routines_not_allowed`), `ops` without the scope (`forbidden`), `agent-a` calling `send_input` (`forbidden`: `routine` does not imply `control`) | tool error codes; only one routine exists afterwards |
| 4 | The first grid time types the text into the `done` pane. A pane that is `working` at its tick gets nothing, `last_result` is `skipped:working`, and nothing arrives when the turn ends later; the next grid time types once | `typed.txt` line counts; `last_result`; `runs` |
| 5 | `SIGUSR2` on the holder while the routine is 25 to 50 s from its tick: the new front lists the same routine on the same grid, and nothing is typed in the first seconds | `next_due_at` differs by a whole number of intervals; line count unchanged |
| 6 | The front is replaced by one that cannot attach (protocol number 999), so MCP is down, for `DOWN_SECS` (default 300); then a matching front attaches | at most one new line in 30 s, not one per missed tick; `next_due_at` still on the grid |
| 7 | The audit file: `routine_create` has `call` and `outcome` under `agent-a`, with the routine id; each nudge is a `send_input` `call` and `outcome` by `system:routine` with `routine` and `created_by`; `routine_list` wrote nothing; the nudge text is nowhere in the file | Python over the JSON lines |
| 8 | `dynamic_routines` is removed from the profile and the holder gets `SIGUSR2`: the routines are `stopped` with `stop_reason` `profile_changed`, nothing types for more than one interval, and `routine_delete` works | `routine_list`, line counts, the log line |
| 9 | The audit file is padded to the holder's size limit, so the next append fails: `routine_create` returns `audit_unavailable`, nothing is created, and `routine_list` still works | error code; the list is empty |
| 10 | The routine created with `expires_secs: 90` has typed once and then shows `expired`, with no next time, and types nothing more | `status`; `next_due_in_secs` null; line count |

Step 9 does not break the audit directory's mode, because an open file keeps working after its
directory changes. A size limit fails the write on demand and on both macOS and Linux.

## With a real agent (optional, costs model prompts)

The script uses no model. To see an agent make the call itself, start a `dyn`-style pane with a real
agent CLI, put a token for a client with the scopes `routine` and `observe` into that CLI's MCP
configuration (never into the pane's environment), and say: "every 2 minutes, check whether any todo
work is left", with a `todo_file` in the pane's directory. Expect one `routine_create`, then
`routine_list` with `next_due_in_secs`. The pane's `echo $SHEPHERD_PANE` prints the name to pass as
`terminal`. Leave a long task running across a tick: nothing types, and `last_result` shows
`skipped:working`.

## What a FAIL usually means

- `the server did not come up within 30 s`: the port is taken (`PORT=<another>`), the binary is the
  wrong architecture, or one of the files was refused. The log tail is printed.
- Step 3 `routine_create ... returns r1` fails with `routines_not_allowed`: the profile has no
  `dynamic_routines`, or the binary is older than the feature (an older binary refuses the profiles
  file at startup instead).
- Step 4, no line at the first grid time: the pane was not `done`, or the pane lock was busy. Read the
  log for `routine nudge` lines.
- Step 6 `MCP is down` fails: the version file was not read by the new front; see
  `version-mismatch.sh` in [restart-survival.md](restart-survival.md).
- Step 6 more than one new line: that is a catch-up burst and a real bug. Keep the log (`KEEP=1`).
- Step 9 does not return `audit_unavailable`: the holder did not apply the size limit (the log shows
  the python start line), or the audit file was already larger than the limit.

## Results for the issue

Paste the `host:`, `binary:`, `summary:` and `RESULT:` lines, and the commit you built. **#98 closes
when this script says `RESULT: PASS` on a macOS host**, and not before: the PRs that carry the code and
the docs say `Refs #98` and do not close it.
