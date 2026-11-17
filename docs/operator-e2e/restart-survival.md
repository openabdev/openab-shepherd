# Restart survival: operator end-to-end scripts

For a person at a terminal on a **macOS host** and a **Linux host**. These scripts start the real
`openab-shepherd hold` and drive it through the MCP tools. **Agents never run them**: an agent must
not start the Shepherd server. Issue #76 is closed; run them on both machines and
pasted the results into the issue (see [Results](#results-for-the-issue)).

None of them calls a model. The panes are `sh -c` scripts that print OSC 7501 status reports and a
recorded Kiro permission dialog, and every verdict comes from MCP fields, files the panes write,
process ids and the holder's log. Only `record-replay-fixture.sh` spends model prompts (two per
agent), and it is a separate step.

Everything is throwaway: a token and profiles file in `~/.config/shepherd-e2e-restart`, pane
working directories and the holder log in `/tmp/shepherd-e2e-restart`, and loopback port 8766
(`PORT=<n>` changes it; it differs from the other runbook's 8765 so both can coexist). Each script
stops its holder, which ends its panes, and removes these files when it ends (`KEEP=1` keeps them).

## Before you start

1. A release binary built on the build host (`cargo build --release`), copied to
   `~/.local/bin/openab-shepherd` on the machine you test (or set `SHEPHERD_BIN`). Build one per
   architecture and note the commit: `git rev-parse --short HEAD`.
2. `bash`, `python3`, `openssl`, `curl` and `pgrep` on the machine. `systemd --user` on the Linux host for
   `service-restart-linux.sh` (over ssh: `loginctl enable-linger $USER` once).
3. Run from this directory (`docs/operator-e2e` in a clone of the repository).

Check the scripts without starting anything: `bash -n *.sh`, and `shellcheck *.sh` where it is
installed.

## The scripts

| Script | What it does | Takes |
|--------|--------------|-------|
| `holder-start.sh` | Starts a holder and leaves it running; prints the pids, the URL and the log. Stop it with `restart-cleanup.sh`. For poking by hand | 5 s |
| `front-kill.sh` | `kill -9` on the front, twice. The holder respawns it (1 s, then 2 s), the pane's program is the same process, `turn` stays 2, status stays `done`, `send_input` still works | 30 s |
| `front-kill-mid-dialog.sh` | Kills the front while two panes show a permission dialog. On the short tail, `answer_dialog` works and the program receives the Enter. Behind more than 1 MiB of output, the pane is `replayed`, `answer_dialog` is refused with `pane_replayed`, and the program receives nothing | 1 min |
| `wrapped-tail.sh` | After a restart, a silent pane with a wrapped tail stays `replayed` and refuses `send_input` for 15 s; a pane that keeps printing leaves `replayed` by itself | 1 min |
| `sigusr2.sh` | Five SIGUSR2 in a row: holder and pane are the same processes, `turn` is kept, no backoff, no defunct front. Prints how long MCP was down each time | 30 s |
| `service-restart-macos.sh` | The holder under launchd: `launchctl kill USR2` keeps the pane, `launchctl kickstart -k` ends it | 1 min |
| `service-restart-linux.sh` | The holder under a systemd user unit: `systemctl --user reload` keeps the pane, `restart` ends it | 1 min |
| `version-mismatch.sh` | A front that speaks another protocol number exits 78 repeatedly with growing delays and the log names both numbers, MCP is down and the pane stays alive; then a matching front adopts it with its state | 1 min |
| `routine-after-restart.sh` | A pane that had used its only nudge stays given up across SIGUSR2; a pane not yet nudged is nudged by the new front at least once and at most `max_consecutive_nudges` times, then gives up and is not nudged again after a second restart; a profile that loses its `routine` block is no longer watched | 2 min |
| `zombie-under-tini.sh` | The image under real tini on the Linux host (Docker): no defunct process, SIGUSR2 through tini, `docker stop`. `NO_TINI=1` is the control and must fail | 1 min |
| `record-replay-fixture.sh AGENT` | Records replay fixtures for `claude`, `codex`, `kiro` or `opencode` with `examples/record_tail.rs` (2 model prompts) | 5 min |
| `restart-cleanup.sh` | Stops a leftover holder and removes the throwaway files | 1 s |

`restart-lib.sh` holds the shared helpers; it is sourced, not run. Each scenario prints `PASS` or
`FAIL` per check, then the host, the binary and `RESULT: PASS` or `RESULT: FAIL`, and exits non-zero
on a failure. After a failure read the holder log (`/tmp/shepherd-e2e-restart/holder.log`, kept only
with `KEEP=1`).

## Running them

On the macOS host, then on the Linux host:

```sh
cd docs/operator-e2e
for s in front-kill front-kill-mid-dialog wrapped-tail sigusr2 version-mismatch routine-after-restart; do
  echo "=== $s"; bash "$s.sh" 2>&1 | tee "/tmp/$s.out"
done
bash service-restart-macos.sh    # macOS host
bash service-restart-linux.sh    # Linux host
IMAGE=<registry>/openab-shepherd:<version> bash zombie-under-tini.sh   # Linux host; on a Mac set DOCKER to the docker binary
```

Run one at a time: they share the port and the throwaway directories. The routine scenario for agent-created routines, `routine-dynamic.sh`, has its own runbook, [routine-dynamic.md](routine-dynamic.md), and its own port and directories. The trusted-client script, `trusted-client.sh` (`extra_args`, `agent`, the Kiro trust-all guard, the audit fields), is described in [trusted-client.md](trusted-client.md).

## What a FAIL usually means

- `the server did not come up within 30 s`: the port is taken (`PORT=<another>`), the binary is the
  wrong architecture, or the profiles file was refused. The log tail is printed.
- `front-kill-mid-dialog.sh`, wrapped pane not `replayed`: the 1.4 MB of filler did not exceed the
  holder's 1 MiB tail. That would be a bug in the tail ring; keep the log.
- `wrapped-tail.sh`, the ticker is still `replayed`: the repaint nudge did not reach a program that
  prints on its own. Keep the log (`nudge` lines come from the front).
- `version-mismatch.sh`, MCP never comes back after the file is emptied: the backoff reached 30 s.
  Wait, or send `kill -USR2` to the holder, which starts the next front at once.
- `routine-after-restart.sh`, `fresh` is never nudged: the new front did not rebuild the watch. Look
  for `its profile is gone or has no routine now` in the log. If `fresh` is nudged again after the second
  restart, the new front lost the nudge counters.

## Results for the issue

Paste, per machine, the last lines of each script (the `host:`, `binary:`, `summary:` and `RESULT:`
lines), the commit you built, and for `sigusr2.sh` the five `MCP back after N ms` lines. Then:

- **#76 (closed)** was gated on `front-kill.sh`, `front-kill-mid-dialog.sh`, `sigusr2.sh`,
  `service-restart-*.sh`, `version-mismatch.sh` and `routine-after-restart.sh` say `RESULT: PASS` on
  a macOS host and a Linux host, `zombie-under-tini.sh` passes on the Linux host, and the agents' fixtures are recorded,
  read and committed under `tests/fixtures/replay/<agent>/` (kiro and opencode are recorded; claude and codex are not). Report results on #77.
- **Mode snapshot.** If a recording shows that replay loses alternate screen, scroll region or
  kitty flags for one of the four agents, say so in the issue. A snapshot of those modes in the pane
  record is a follow-up, not part of #76.

## Fixtures

```sh
# on the build host
cargo build --release --example record_tail
# on a machine where the agent CLI is logged in
bash record-replay-fixture.sh kiro
```

`record_tail` starts the command on a native pane, waits for quiet, performs `--send` and `--key`
steps, and writes `NN-<scenario>.raw`, `.screen.txt` and `.json`. It replaces your home directory
with `/home/user` and refuses to write when the bytes contain a `Bearer` header, an `sk-` or `ghp_`
key, or `token=<value>`. It cannot know what else is on the screen: read each file before you commit
it. Its own tests run with `cargo test --example record_tail`.
