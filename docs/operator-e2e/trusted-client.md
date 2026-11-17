# Trusted clients: operator end-to-end runbook

For a person at a terminal on a **macOS host** or a **Linux host**; the script runs on both and no
step is specific to one of them. `trusted-client.sh` starts the real `openab-shepherd hold` and
drives `start_pane`, `send_input`, `answer_dialog` and `get_status` through MCP with three clients
of different trust. **Agents never run it**: an agent must not start the Shepherd server. Issues
#113 and #111 stay open until you have run it and pasted the result into them (see
[Results](#results-for-the-issues)).

It calls no model. The panes are `sh -c` scripts that print OSC 7501 status reports, Kiro's
trust-all warning or a recorded Kiro permission dialog, and write what they were given to files.
The checks come from MCP fields, those files and the audit file. Read the
[trusted-client recipe](../adr/trust-model.md#trusted-clients) and
[Call-level `extra_args`](../operations.md#call-level-extra_args-for-a-profile-start) first.

Everything is throwaway: tokens, a clients file, the audit file and a profiles file in
`~/.config/shepherd-e2e-restart-trusted`, pane working directories and the holder log in
`/tmp/shepherd-e2e-restart-trusted`, and loopback port 8768 (`PORT=<n>` changes it; it differs from
the other runbooks' ports). The script stops its holder, which ends its panes, and removes these
files when it ends (`KEEP=1` keeps them).

**One exception to the safety rules in [README.md](README.md#safety-rules):** this script writes
`allow_arbitrary_command: true` into its throwaway profiles file, because the `agent` steps start a
program of the caller's choosing. The server listens on loopback only, the tokens are in a mode 700
directory outside every repository, and the file is deleted at the end. It never uses `--auto`,
`autoaccept` or `bypass` on a real agent.

## Before you start

1. A release binary built on the build host, copied to `~/.local/bin/openab-shepherd` (or set
   `SHEPHERD_BIN`). Note the commit you built from. It must contain the Kiro trust-all guard, the
   `agent` field and the `start:args` scope (the step 2 to 5 checks fail on an older binary).
2. `bash`, `python3`, `openssl`, `curl` and `pgrep` on the machine.
3. Run from this directory (`docs/operator-e2e` in a clone of the repository).
4. Check the script without starting anything: `bash -n trusted-client.sh`, and
   `shellcheck -x trusted-client.sh` where it is installed.

## Run it

```sh
cd docs/operator-e2e
bash trusted-client.sh 2>&1 | tee /tmp/trusted-client.out
```

It takes about two minutes.

## What it sets up

- Profiles `sandbox` (Kiro; records the arguments it was started with, then what is typed),
  `guard` (Kiro; shows the trust-all warning with `No, exit` as the default and reads nothing until
  a file `accept` exists), `other` (Kiro; never on the orchestrator's list) and `oc` (OpenCode).
  The profiles file has `allow_arbitrary_command: true`.
- Three clients: `ops` (`observe`, `read`, `control`, `start`, `remove`; no `start:args`),
  `orch` (the same plus `start:args`; profiles `sandbox`, `guard`, `oc`) and `trusted` (adds
  `start:arbitrary` and `dialog`; profile `sandbox`).
- An audit file in a directory of mode 700.

## The checks

| # | What happens | What proves it |
|---|--------------|----------------|
| 1 | Holder, clients file, audit file and profiles start | MCP answers under the `ops` token |
| 2 | `orch` starts `sandbox` with `extra_args ["-a", "--model", "claude-sonnet-4"]`: the returned `argv` ends with them, after the profile's own arguments, and the pane received them. `ops` (no `start:args`) is `forbidden` and the error names the scope; a profile off the list is `forbidden`; `start:arbitrary` does not stand in for `start:args`; `null` and `[]` are accepted without the scope and add nothing | `argv` in the reply; `args.txt` in the pane directory; `error` and `scope` |
| 3 | Refusals, each `invalid_input` and none leaves a pane: a control character, an empty element, an element over 4096 bytes, more than 256 arguments, `extra_args` without a profile, `extra_args` with `argv`, and `--auto` on an OpenCode profile (with the `field` `extra_args`) | error code; no pane named `rej` in `list_panes` |
| 4 | The Kiro trust-all guard. The `guard` pane shows the warning; `send_input` is `pane_needs_trust_acceptance` with the pane named, nothing is typed, the pane is still live and `answer_dialog` is refused. After the warning is gone (the script creates `accept`, which stands for a person accepting it) a `send_input` works and the pane gets exactly that line | `typed.txt` absent for a window, then equal to the text; `get_status` `live` |
| 5 | `agent` on an argv start. `ops` is `forbidden` for `argv`. `trusted` starts the same dialog program twice, with and without `agent: "kiro"`: with it `answer_dialog allow_once` reaches the pane, without it the call is `refused` and no key arrives. `agent` with a profile and an unknown agent are `invalid_input` | `received.txt`; error codes |
| 6 | The audit file. The `orch` start records `launch` `profile+args`, `extra_args_n` and `flags` (flag names only); the argv start records `launch` `argv`, `argv0` and `agent`; the start without `agent` records none; the denied start is a `denied` line; a plain profile start has no `launch`. No argument value is in the file or in the holder log: a canary in a flag value, in an attached short flag value and after `=` never appears, nor the model name, the program text or the typed text. Canaries also sit in the refused calls (denied, profile not listed, control character, auto-approve flag), and the replies to those calls must not repeat them | Python over the JSON lines, with a bounded wait for the lines |

Every read of something a pane or the server writes after the call returned waits, up to 20 s
(`t_until`). A "nothing happened" check watches a 3 second window (`t_stays`) after the pane has
shown that it is ready, so a result is never read before it can exist, and an absence is not read
before the pane could have written it.

The audit file full (`audit_unavailable`) is not checked here. It needs a write that fails on
demand, which is Linux only (macOS does not enforce `RLIMIT_FSIZE`); it is a step of
[routine-dynamic.md](routine-dynamic.md), and the behaviour is covered by the unit tests. This
script has no Linux-only or macOS-only step; if one is added it must print a `SKIP` line, which the
summary counts (`N passed, 0 failed, 1 skipped`) and which never decides `RESULT`.

## By hand, with a real kiro-cli (answers #111)

The script cannot measure what only the real CLI shows. On a macOS host with kiro-cli 3.3.0 (and
later the image's 3.1.0), in a throwaway directory and a throwaway `HOME`, with a profile you started
the usual way:

1. **The choice persists (measured on 3.3.0).** `Yes, and don't ask again` writes
   `chat.disableTrustAllConfirmation: true` to `$HOME/.kiro/settings/cli.json`, and the next
   `kiro-cli chat -a --trust-workspace` with the same HOME shows no warning. `Yes, I accept`
   persists nothing. After either choice the phrase `trust all tools mode` is gone within a
   second and the footer reads `Trust All Tools active, confirmations are off`. To re-check on a
   new Kiro version, repeat this in a throwaway HOME (a new empty directory), never your real one.
2. **The guard on the real screen.** A profile with `"extra_args": ["--trust-workspace", "-a"]`
   (or `start_pane` with `extra_args ["-a"]` from a client that holds `start:args`) and a pane HOME
   without the persisted key: `start_pane`, then `send_input "hello"` is
   `pane_needs_trust_acceptance` and `list_panes` still shows Kiro alive. The committed fixture
   `tests/fixtures/kiro-3.3.0/trust-all-warning.screen.txt` is that screen as measured; it cannot
   be re-recorded with `record-replay-fixture.sh`, which does not pass `-a`. Attach, accept the
   warning, and `send_input` works. With the key persisted in the pane's HOME there is no warning
   and `send_input` works at once, also after a restart.
3. **`agent` with a real Kiro.** `start_pane {argv: ["kiro-cli", "chat", "--trust-workspace"],
   agent: "kiro", cwd}` from a client with `start:arbitrary`: one model prompt that asks for a
   shell command, `wait_status` is `blocked` and `answer_dialog allow_once` works. Without
   `agent` the same call is refused and sends no key.
4. **Refusals on a Devin profile.** `extra_args ["--permission-mode", "dangerous"]` is
   `invalid_input`.
5. **Compatibility.** An existing script of this directory runs unchanged against the new binary.
   Against an old server, a new client reads the returned `argv` and sees that `-a` is missing.

## What a FAIL usually means

- `the server did not come up within 30 s`: the port is taken (`PORT=<another>`), the binary is the
  wrong architecture, or the clients file names a scope the binary does not know (a binary older
  than `start:args`; the log tail is printed).
- Step 2 `extra_args are appended` fails with an `argv` that lacks the flags: the binary drops
  unknown arguments (older than this feature). Read the returned `argv`.
- Step 4 `the screen shows the warning` fails: the pane printed nothing yet, or the binary has no
  `screen_text`. A guard that does not refuse shows as `send_input` succeeding and a line in
  `typed.txt` before `accept`.
- Step 5 `answer_dialog allow_once works` fails: the pane was not `blocked` yet (the check waits
  for it), or the `agent` field was ignored by an older binary.
- Step 6 `no argument value is in the file` fails: an argument value reached the audit file. Keep
  the log and the audit file (`KEEP=1`) and report it: that is a real bug.

## Results for the issues

Paste the `host:`, `binary:`, `summary:` (with its skipped count) and `RESULT:` lines, the answers
to the by-hand steps above (in particular whether `Yes, and don't ask again` persists), and the
commit you built. **#113 and #111 close when this script says `RESULT: PASS` on a host of your
choice and the by-hand steps 1 and 2 are answered**, and not before: the PRs that carry the code and
the docs say `Refs #113` and do not close it.
