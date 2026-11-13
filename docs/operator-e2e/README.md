# OpenAB Shepherd: operator end-to-end runbook (Devin + OpenCode)

For a human at a macOS terminal (zsh). It drives a real Shepherd server with real Devin and OpenCode panes through the MCP tools, and decides every verdict from files and fields, not from model text. The helper scripts live next to this file: `setup.sh`, `start-server.sh`, `ready.sh`, `mcp.sh`, `check_profiles.py`, `cleanup.sh`.

Run the scripts from this directory (`docs/operator-e2e` in a clone of the repository). The scripts use `stat -f`, so they target macOS.

Validated against `origin/main` commit `afd73c9`; see [Known results](#known-results) and [Verified against](#verified-against).

## Safety rules

These hold for every step. Do not relax them for a test.

- Never use `--auto`, `autoaccept`, `bypass`, or `allow_arbitrary_command: true` anywhere.
- The token file has mode 0600 (owner only) and lives outside every git repository and outside the folder the server starts from. The token is never printed, never placed on a command line, and never written into a working directory.
- The server listens on loopback only.
- Nothing here logs in to an agent. Only check the login state.
- The throwaway working directories hold no credentials or keys.

## Budget

Model prompts sent to Devin/OpenCode in the whole runbook: **4 at most, 2 at minimum**. Every other step costs no model call (login checks, trust prompt, refusals, `/yolo`, `read_output`, `ps`).

| # | Prompt | Check | Required? |
|---|--------|-------|-----------|
| 1 | Devin: run `touch .../marker-allow` | D2 (allow) | required |
| 2 | Devin: run `touch .../marker-deny` | D3 (deny) | optional |
| 3 | OpenCode: run `touch .../marker-oc-allow` | OC2 (allow, styled-cell Enter) | required |
| 4 | OpenCode: run `touch .../marker-oc-deny` | OC3 (deny, Esc) | optional |

One prompt may make the agent run a follow-up command (OpenCode did `ls -l marker` once). That is a second dialog inside the same prompt, not a second prompt. Provider-side that is more than one model API call; you cannot control it.

Steps that need you (cannot be automated): starting the server in its own terminal, the one-time Devin trust answer (D0), reading the screens named under "look at", and the final cleanup.

## 0. Preconditions (no login, no model call)

Do not log in again. Only check.

```zsh
devin auth status            # expect: Logged in
devin --version              # record yours (live runs used 3000.12.57)
opencode --version           # record yours (live runs used 2.2.7)
opencode auth list           # expect a provider row (values are not printed)
python3 - <<'PY'             # prints only the model name, nothing secret
import json, os
print(json.load(open(os.path.expanduser("~/.config/opencode/opencode.json"))).get("model"))
PY
command -v devin opencode git python3 openssl curl
```

Shepherd release binary. The project rule is that the machine you work on does not run `cargo` or `rustc`. The maintainer builds on a separate machine and copies the binary back. Any build host works; the pattern is the same (`BUILD_HOST` is an ssh alias you define for it, for example a Mac for macOS binaries):

```zsh
git clone https://github.com/openabdev/openab-shepherd.git
cd openab-shepherd
git fetch origin && git checkout --detach origin/main
git rev-parse --short HEAD                                 # note the commit you test
ssh BUILD_HOST 'mkdir -p ~/openab-shepherd-e2e'
rsync -a --delete --exclude .git --exclude target ./ BUILD_HOST:openab-shepherd-e2e/
ssh BUILD_HOST 'cd ~/openab-shepherd-e2e && export PATH=$HOME/.cargo/bin:$PATH && cargo build --release'
mkdir -p ~/.local/bin
scp BUILD_HOST:openab-shepherd-e2e/target/release/openab-shepherd ~/.local/bin/openab-shepherd
~/.local/bin/openab-shepherd --help | head -3              # prints usage, starts nothing
```

If the build host's target directory is on an external volume, a build over ssh still works.

## 1. Token, directories, profiles

```zsh
cd docs/operator-e2e        # from the repository root
./setup.sh
```

`setup.sh` (umask 077) does the following; read it before running:

- Token: `openssl rand -hex 32` into `~/.config/shepherd-e2e/token` (64 chars; the server needs at least 16), `chmod 600`. The directory is 700 and is checked to be outside any git repository. Verify:

  ```zsh
  stat -f '%Lp' ~/.config/shepherd-e2e/token       # expect: 600  (0600 or stricter; any group/other bit = server exits 2)
  git -C ~/.config/shepherd-e2e rev-parse --show-toplevel   # expect: fatal: not a git repository
  ```
- Profiles file `~/.config/shepherd-e2e/profiles.json`. Required mode: regular file, owned by you, **not writable by group or others** (`chmod 600` or `644`; `setup.sh` uses 600). It must also be outside any git repository and outside the folder the server starts from. Content (paths are filled with `command -v` results):

  ```json
  {
    "allow_arbitrary_command": false,
    "profiles": [
      { "name": "devin", "agent": "devin", "permission_mode": "normal",
        "argv": ["/abs/path/to/devin"], "cwds": ["/tmp/shepherd-e2e/devin"] },
      { "name": "opencode", "agent": "opencode", "permission_mode": "normal",
        "argv": ["/abs/path/to/opencode", "--standalone"], "cwds": ["/tmp/shepherd-e2e/opencode"] }
    ]
  }
  ```

  Notes: a `devin` / `normal` pane runs `devin --permission-mode auto` (Shepherd adds the flag; never put `--permission-mode` in argv). OpenCode needs `--standalone`, otherwise it leaves a background server running. `cwds` must exist when the server starts.
- Second file `bad-profiles.json`: the OpenCode profile with `--auto` appended. For check R2 only.
- `check_profiles.py` re-checks `profiles.json` against the rules in `src/profiles.rs` (unknown fields, name pattern, absolute `argv[0]`, cwds exist, no auto flags, no `--permission-mode`, mode bits). It prints `profiles file passes the documented rules`. It is only an early warning; the server's own load is the authority.
- Working directories (mode 700): `/tmp/shepherd-e2e/devin`, `/tmp/shepherd-e2e/opencode`, `/tmp/shepherd-e2e/server` (the server's own start folder). One directory per agent, reused by its checks; markers have distinct names.
- `/tmp/shepherd-e2e/opencode/opencode.jsonc` (this is why OpenCode shows dialogs; **without `ask` rules OpenCode runs shell commands with no dialog, no `blocked` state, and Shepherd has nothing to answer**):

  ```json
  {
    "$schema": "https://opencode.ai/config.json",
    "permissions": [
      { "action": "shell", "resource": "*", "effect": "ask" },
      { "action": "edit", "resource": "*", "effect": "ask" }
    ]
  }
  ```

  No `"model"` key is set, so your own default model is used.

### D0: trust the Devin directory once (no model call)

Shepherd never answers Devin's trust prompt; it closes the pane and returns `trust_prompt`. Trust the directory first:

```zsh
cd /tmp/shepherd-e2e/devin && devin --permission-mode auto
# at "Do you trust the authors of this directory?" choose 1 (Yes, trust). When the input box shows, press Ctrl-C twice.
```

OpenCode showed no trust prompt on a fresh directory in the live measurement.

## 2. Refusal checks that need no server and no model (R1, R2)

Run these in a second terminal before starting the real server. Each must exit with status 2 and **no process stays listening**. Port 8766 is used so that even a wrong result cannot collide with the real server. If the server unexpectedly starts, press Ctrl-C and mark FAIL.

```zsh
cd /tmp/shepherd-e2e/server
SH=$HOME/.local/bin/openab-shepherd; T=$HOME/.config/shepherd-e2e

# R1: the removed hc backend
OPENAB_SHEPHERD_BACKEND=hc OPENAB_SHEPHERD_TOKEN_FILE=$T/token OPENAB_SHEPHERD_PROFILES=$T/profiles.json \
  $SH --host 127.0.0.1 --port 8766; echo "exit=$?"
# expect stderr: openab-shepherd: refusing to start: OPENAB_SHEPHERD_BACKEND=hc is no longer supported ...   exit=2

# R2: OpenCode profile with --auto must be refused at load (separate profiles file)
OPENAB_SHEPHERD_TOKEN_FILE=$T/token OPENAB_SHEPHERD_PROFILES=$T/bad-profiles.json \
  $SH --host 127.0.0.1 --port 8766; echo "exit=$?"
# expect stderr: ... profile opencode: argv and extra_args may not pass --auto for opencode ...   exit=2
```

Then, from `docs/operator-e2e`:

```zsh
python3 ./check_profiles.py $T/bad-profiles.json --expect-refused
```

PASS: `exit=2` and the message names `hc` / `--auto`. FAIL: any other exit status, or a `listening on` line.

## 3. Start the server (its own terminal, "terminal A")

Either run `./start-server.sh` (it checks the binary, the token and profile modes, that `devin` and `opencode` are on PATH, that the backend variable is unset or `native`, and that the port is free; set `PORT=<n>` to use another port), or start it by hand:

```zsh
cd /tmp/shepherd-e2e/server
OPENAB_SHEPHERD_TOKEN_FILE=$HOME/.config/shepherd-e2e/token \
OPENAB_SHEPHERD_PROFILES=$HOME/.config/shepherd-e2e/profiles.json \
$HOME/.local/bin/openab-shepherd --host 127.0.0.1 --port 8765
```

Expected stderr: `openab-shepherd: listening on http://127.0.0.1:8765/mcp (backend: native)`. Leave it running; panes are its children and end with it. `OPENAB_SHEPHERD_BACKEND` is not set (unset, empty or `native` all work). The server's PATH is passed to panes, so start it from a shell where `devin` and `opencode` work.

Do not use a port that another Shepherd server already holds.

## 4. MCP helper (terminal B)

`./ready.sh` first checks that the server answers 401 without the token and 200 with it (no model call, no token on a command line). Then:

```zsh
cd docs/operator-e2e     # from the repository root
source ./mcp.sh          # works in zsh and bash
W=/tmp/shepherd-e2e
mcp_init                 # expect: initialized: openab-shepherd <version> protocol 2025-06-18
list_panes | expect live_panes=0
```

To use another port, set `SHEPHERD_URL` (default `http://127.0.0.1:8765/mcp`) before sourcing.

Functions: `start_pane PROFILE CWD NAME`, `send_input NAME TEXT`, `wait_status NAME [SECONDS]`, `read_output NAME [LINES]`, `read_screen NAME` (plain text), `get_status NAME`, `answer_dialog NAME allow_once|deny_once`, `remove_pane NAME [SECONDS]`, `list_panes [true]`, generic `mcp_tool TOOL key=value key:=JSON`. `expect path=value ...` reads a JSON reply on stdin, prints it, and prints PASS/FAIL per field on stderr. `expect_file` / `expect_no_file` check marker files. The token is read from the token file and given to curl on stdin; it is never printed. A tool error (`isError`) prints its `structuredContent` and returns 1. HTTP 401 = wrong token, 403 = Host problem (the helper says so).

Mode of use: run each block, read the JSON, then read the PASS/FAIL lines.

## 5. Devin checks (pane name `dv`)

### D1: start reaches idle, no trust prompt

```zsh
start_pane devin $W/devin dv
sleep 5
get_status dv | expect live=true reported=true status.state=idle
ps -axo pid=,args= | grep -- '[-]-permission-mode'     # look at: the devin line ends with "--permission-mode auto"
```
Expected: `start_pane` returns a pane object (`name: dv`, `live: true`, `cwd` under `/private/tmp/shepherd-e2e/devin`); no error `trust_prompt`. The immediate reply can show `reported: false`; after a few seconds `status.state` is `idle` and `reported` is `true`. PASS: both, and the `ps` line shows `auto`. FAIL: `trust_prompt` (redo D0), `reported: false` that does not change (the native backend did not turn Devin's OSC 7501 into a status), or any state other than `idle`.

### D5: `/yolo` refusal, no model call

```zsh
send_input dv '/yolo' | expect error=refused
read_screen dv | grep -ci yolo          # expect 0: nothing was typed
get_status dv | expect status.state=idle
```
PASS: `error=refused`, message mentions the Devin mode-switch rule, grep prints `0`, state still `idle`. FAIL: any text reached the pane. Run this before the prompts so a failure does not leave Devin in another mode.

### D2: allow (prompt 1)

```zsh
send_input dv "Use the shell tool to run exactly: touch $W/devin/marker-allow . Do nothing else." | expect started=true
wait_status dv 120 | expect outcome=blocked kind=permission turn_complete=false
read_screen dv                          # look at: "Running command", "$ touch .../marker-allow", rows Allow / Bypass mode / Deny, highlight on Allow
expect_no_file $W/devin/marker-allow    # not yet: the dialog is waiting
answer_dialog dv allow_once | expect agent=devin choice=allow_once selected=Allow moves=0
wait_status dv 120 | expect outcome=done turn_complete=true
expect_file $W/devin/marker-allow
```
PASS: every line PASS; `command` in the `answer_dialog` result names `marker-allow`. If the second `wait_status` returns `blocked` again (the model ran a follow-up), read `command`; answer `allow_once` only for a harmless command inside the directory, otherwise `deny_once`, then wait again. FAIL: `send_input` text stays in the input box with no `working` state (`wait_status` times out with status `idle`), `answer_dialog` returns `refused`, or the marker is missing.

### D3: deny (prompt 2, optional)

```zsh
send_input dv "Use the shell tool to run exactly: touch $W/devin/marker-deny . Do nothing else." | expect started=true
wait_status dv 120 | expect outcome=blocked kind=permission
answer_dialog dv deny_once | expect choice=deny_once selected=Deny moves=2
wait_status dv 60 | expect outcome=cancelled status=idle turn_complete=false
expect_no_file $W/devin/marker-deny
get_status dv | expect status.state=idle
```
PASS: all lines PASS (`moves=2`: two Down presses from Allow past Bypass mode to Deny; Enter only on Deny). The result is `cancelled`, not `done`. The `answer_dialog` result for Devin has `moves`, `options`, `selected` and `keys_result`, no `keys` field.

### D4: Bypass never chosen, `dangerous` never in argv

```zsh
ps -axo args= | grep -c 'permission-mode [d]angerous'      # expect 0
ps -axo args= | grep -- '[-]-permission-mode auto'         # expect one devin line
get_status dv | python3 -c 'import json,sys; print(json.load(sys.stdin)["argv"])'   # look at: no "dangerous"
```
PASS: count `0`, one `auto` line, and in D2/D3 the `selected` fields were `Allow` / `Deny`, never `Bypass mode`. FAIL: any `dangerous`, or `selected` equal to `Bypass mode`.

Then remove the pane: `remove_pane dv | expect was_live=true` (`stopped` is `exited` or `killed`).

## 6. OpenCode checks (pane name `oc`)

### OC1: start in the directory that has the ask rules

```zsh
ls $W/opencode/opencode.jsonc
start_pane opencode $W/opencode oc
sleep 8
get_status oc | expect live=true reported=true status.state=idle
ps -axo args= | grep -c '[o]pencode.*--auto'               # expect 0
read_screen oc                                             # look at: the OpenCode prompt box, no trust prompt
```
The `[o]` bracket keeps the `grep` command line from matching itself; a plain `grep -c 'opencode.*--auto'` counts its own process and prints 1. To list the OpenCode processes with their arguments, use `pgrep -fl '[o]pencode'`.

PASS: `idle` and `reported=true`, count `0`. FAIL: `reported=false` that does not change, or a startup prompt.

### OC2: allow (prompt 3)

```zsh
send_input oc "Run this shell command and nothing else: touch $W/opencode/marker-oc-allow" | expect started=true
wait_status oc 120 | expect outcome=blocked kind=permission
read_screen oc                          # look at: "△ Permission required", "$ touch .../marker-oc-allow", "Allow once  Always allow  Reject  ctrl+f fullscreen"
expect_no_file $W/opencode/marker-oc-allow
answer_dialog oc allow_once | expect agent=opencode choice=allow_once selected='Allow once'
wait_status oc 120 | expect outcome=done turn_complete=true
expect_file $W/opencode/marker-oc-allow
```
Expected: `answer_dialog` presses Enter after verifying the highlight from styled cells; the result has `keys: ["enter"]`; the pane goes `working` then `idle`, and the outcome is `done`. PASS: all lines PASS. If the model asks for a second command (for example `ls`), handle as in D2. FAIL: `answer_dialog` returns `refused` (record `reason` and the `seen` screen: nothing was pressed, so clean up with `answer_dialog oc deny_once`), `wait_status` never leaves `blocked`, the outcome is `cancelled` although the screen shows the command ran, or no marker.

### OC3: deny (prompt 4, optional)

```zsh
send_input oc "Run this shell command and nothing else: touch $W/opencode/marker-oc-deny" | expect started=true
wait_status oc 120 | expect outcome=blocked kind=permission
answer_dialog oc deny_once | expect choice=deny_once selected=Reject
wait_status oc 60 | expect outcome=cancelled status=idle turn_complete=false
expect_no_file $W/opencode/marker-oc-deny
get_status oc | expect status.state=idle
```
PASS: all lines PASS (`keys: ["esc"]`, never `Always allow`). Then `remove_pane oc | expect was_live=true`.

## 7. Results table (copy and fill)

| Check | Expected | Observed | PASS/FAIL |
|-------|----------|----------|-----------|
| R1 `OPENAB_SHEPHERD_BACKEND=hc` | exit 2, message names hc | | |
| R2 OpenCode `--auto` profile | exit 2, message names `--auto` | | |
| Server start | `listening on http://127.0.0.1:8765/mcp (backend: native)` | | |
| D1 | `idle`, `reported=true`, no `trust_prompt`, argv `auto` | | |
| D5 `/yolo` | `error=refused`, nothing typed, `idle` | | |
| D2 allow | `blocked`/`permission`, `allow_once`, marker-allow exists, `done` | | |
| D3 deny (optional) | `deny_once`, marker-deny absent, `cancelled`/`idle` | | |
| D4 | no `dangerous`, `selected` never `Bypass mode` | | |
| OC1 start | `idle`, `reported=true`, no `--auto` | | |
| OC2 allow | `blocked`/`permission`, Enter pressed, marker-oc-allow exists, `done` | | |
| OC3 deny (optional) | Esc, marker-oc-deny absent, `cancelled`/`idle` | | |
| Cleanup | no panes, no listener, files gone | | |
| Prompts used (max 4) | | | |
| Devin / OpenCode / binary versions | | | |

## 8. Cleanup

```zsh
remove_pane dv; remove_pane oc            # skip a pane that is already removed (not_found)
list_panes | expect live_panes=0
# terminal A: press Ctrl-C (the server ends its panes on SIGINT)
lsof -nP -iTCP:8765 -sTCP:LISTEN          # expect: no output
pgrep -fl '[d]evin|[o]pencode'            # expect: nothing from these checks
cd docs/operator-e2e                      # from the repository root
./cleanup.sh                              # removes /tmp/shepherd-e2e and the token / profiles files
ls ~/.config/shepherd-e2e 2>&1            # expect: No such file or directory
```
The binary `~/.local/bin/openab-shepherd` and the copy on the build host stay; delete them if unwanted.

## 9. Troubleshooting

- **Devin version drift**: the CLI auto-updates. The parser follows the dialog layout of 3000.12.57; if `answer_dialog` refuses with a layout reason, note the version (`devin --version`) and the `seen` screen.
- **`trust_prompt` error from `start_pane`**: the directory is not trusted yet; redo D0 for that exact directory. A trust prompt that appears more than about 3 s after the start is not detected; `answer_dialog` then finds no dialog.
- **OpenCode never blocks**: the `ask` rules are missing or not read. Check `/tmp/shepherd-e2e/opencode/opencode.jsonc` exists and the pane's cwd is that directory (`get_status oc`, field `cwd`). Without `ask` rules the command just runs and `wait_status` reports `done` with no dialog.
- **HTTP 401** (`unauthorized`): the token file differs from the one the server loaded. Restart the server or fix `SHEPHERD_TOKEN_FILE`.
- **HTTP 403** (`forbidden`): the Host header is not `127.0.0.1`, `[::1]` or `localhost` (with or without a port). Use the default `SHEPHERD_URL`.
- **Server exits 2 at start**: stderr says why. Typical: token or profiles file inside a git repository or inside the server's start folder, token mode wider than 0600, profiles file group/other-writable, a `cwds` entry that does not exist, token shorter than 16 characters.
- **`start_pane` returns `not_found`**: `argv[0]` is not an executable at that path (re-run `setup.sh` after fixing `PATH`), or the cwd is outside the profile's `cwds`.
- **`send_input` text sits in the input box**: Shepherd pastes the text and then presses Enter; if `started` is false and the state stays `idle`, record it and read the screen.
- **`wait_status` returns `timeout`**: a check-in only; call it again.

## Known results

A live run on `origin/main` `afd73c9` (native backend, Devin 3000.12.57, OpenCode 2.2.7, 2 model prompts) passed every check in the matrix below. Earlier runs of this runbook on older commits found four defects, each fixed on `main` before that run:

1. Devin `answer_dialog allow_once` was refused because the dialog layout of 3000.12.57 did not parse (fixed in #34).
2. OpenCode `answer_dialog allow_once` was refused because the styled-cell highlight check did not find the selected option (fixed in #35 with a palette-independent relative rule).
3. OpenCode `wait_status` after `allow_once` reported `cancelled` with `turn_complete=false` although the command ran (fixed in #36: it now reports `done`).
4. Devin `answer_dialog deny_once` was refused by the highlight check after the first Down press, because the highlight was not re-read after each key (fixed in #37). Only Down and Esc had been sent; Enter was never sent and the marker was not created.

Passing matrix on `afd73c9`:

| Check | Result |
|-------|--------|
| Devin start | idle, `reported=true`, argv `devin --permission-mode auto`, no `trust_prompt` |
| Devin deny | `blocked`/`permission`, `deny_once` gives `selected=Deny`, `moves=2`; then `cancelled`, `idle`, `turn_complete=false`; marker absent |
| Devin never Bypass | `selected` was `Deny`; no `dangerous` in argv |
| OpenCode start | idle, `reported=true`, argv `opencode --standalone`, no `--auto` |
| OpenCode allow | `blocked`/`permission`, `allow_once` gives `selected="Allow once"`, `keys: ["enter"]`; then `done`, `turn_complete=true`; marker exists; no follow-up command |
| Cleanup | both panes removed, `live_panes=0` |

The `/yolo` refusal, the Devin allow path, the Devin and OpenCode starts, and the OpenCode deny path (`selected=Reject`, `keys: ["esc"]`, `cancelled`) were checked on `d7b7957`, the commit before the last two fixes, with the same outcomes (Devin allow: `selected=Allow`, `moves=0`, `done`). The two refusal checks R1 and R2 need no server and no model; run them on every pass.

## Verified against

`origin/main` commit `afd73c9` (the fixes up to #37). The command names, flags, environment variables, tool names and config keys in this runbook were checked against the code and docs of that commit:

| Detail | Where to look |
|--------|---------------|
| Env names `OPENAB_SHEPHERD_TOKEN_FILE`, `_BACKEND`, `_ALLOWED_HOSTS`, `_PROFILES`; flags `--host`, `--port`; exit status 2 on refusal; startup line | `src/config.rs`, `src/profiles.rs`, `src/main.rs` |
| Token rules: mode, at least 16 chars, outside git repo and the start folder | `src/config.rs`; `docs/operations.md` |
| Backend: unset/empty/`native` accepted, `hc` refused | `src/config.rs` |
| Profiles file: regular file, owner, no group/other write, 1 MiB limit, unknown fields refused; OpenCode `--auto` refused; Devin `--permission-mode` forbidden in argv and added by Shepherd | `src/profiles.rs`; `docs/operations.md` |
| HTTP: `POST /mcp`, Host check (403), bearer (401), 202 for notifications | `src/http.rs` |
| `initialize`, `tools/call`, `structuredContent`, `isError` | `src/mcp.rs` |
| Tool names and arguments; result fields of `wait_status`, `send_input`, `answer_dialog`, `list_panes`; the `/yolo` refusal | `src/tools.rs` |
| Pane object fields | `src/backend.rs` |
| Dialog labels `Allow`, `Bypass mode`, `Deny` (Devin); `Allow once`, `Always allow`, `Reject` (OpenCode) | `src/devin_dialog.rs`, `src/opencode_dialog.rs` |
| OpenCode `--standalone`, ask rules, `cancelled`/`done` mapping, styled-cell Enter | `README.md` (OpenCode CLI section) |

Not covered by the run: whether a parent directory's Devin trust covers subdirectories (the runbook trusts the exact directory), and a trust prompt for OpenCode in other directories.
