# ADR: Native terminal core (NativeBackend)

- **Status:** Accepted
- **Date:** 2026-11-10
- **Author:** chaodu-agent
- **Based on:** the `TerminalBackend` trait in `src/backend.rs` as first read on branch `feat/backend-trait` at `9022867b70160a9dce89d2a4906a5ceb6fbb4205`. That PR merged as #5; the implementation targets `main` at `091aac3`. The differences between the two are listed in §13, "Reconciliation with the merged trait".
- **Related:** issue #2 (backend trait), `.agents/tasks/kiro-osc7501-without-hc/report.md`, `.agents/tasks/openab-pty-backend-gap/report.md`, `.agents/tasks/kiro-blocked-probe/report.md`, openabdev/openab `.agents/tasks/openab-shepherd-osc7501-probe/report.md`, openabdev/openab-pty `runtime/src/capproxy.rs` at `d61521d` (ideas only, no code copied).

Labels used below: **measured** means observed in a probe report, in code, or in a crate's source. **decided** means chosen here. **open** means an open question, listed in §15.

---

## 1. Context

Shepherd runs every pane through herdr-core (hc) over its Unix socket. The trait from issue #2 puts hc behind `TerminalBackend`, so the MCP tools, input rules, pane lock and dialog parser no longer depend on hc. The owner wants Shepherd off hc as fast as possible, and chose to build a terminal core inside Shepherd rather than extend openab-pty. openab-pty has no screen model, no program status and no programmatic control plane (gap report, measured), and closing that gap means reversing decisions recorded in openabdev/openab.

What a replacement must provide comes from what the tools use today (gap report, measured; hc 0.1.19 OpenRPC schema, measured):

- `run` with a client-chosen `argv` or `line`, a `cwd` and a `name` that is unique among live terminals.
- `read`: the rendered screen, plus `lines` rows of history above it.
- Program status from OSC 7501, with `reported`, `turn` and `status_ms`.
- `send`: paste the text, then press Enter. It refuses while the program is blocked.
- `keys`: named keys, encoded for the program's keyboard modes.
- `wait`: returns at once if the program is not working. After a send, it waits for the turn that send started.
- `interrupt`: Esc if the program reports status, else Ctrl+C. It observes the effect and never escalates.

Facts about kiro-cli 3.1.0 (V3) this design relies on:

- Kiro emits `ESC ] 7501 ; state=<word> : app=kiro-cli BEL` unconditionally in a plain PTY. It sends no 7501 query and reads no `HC_*` env (measured).
- It reports `idle` once at startup (about 3.5 to 4 s after launch), `working` about 17 ms after Enter, and `done` at the end of a turn. It stays `done` until the next `working` (measured).
- Under hc, a tool permission prompt shows as `state=blocked`, `kind=permission`, and the turn goes up on it (measured through hc). The raw bytes of the blocked report were never captured (**open**, Q1).
- Kiro reports `blocked` before the dialog is drawn (measured). `answer_dialog` already waits up to 3 s for it to render.
- At startup Kiro sends `OSC 11 ; ?`, the kitty query `CSI ? u` and CPR `CSI 6 n`. It ran fine with `?u` and XTVERSION unanswered (measured).
- Each status report sits inside a redraw frame, just before `CSI ? 2026 h` (synchronized output), in the middle of a read chunk (measured).

## 2. Decision summary

| Topic | Decision |
|---|---|
| PTY | `libc` directly: `openpty` plus `std::process::Command` with a `pre_exec` that calls `setsid` and `TIOCSCTTY`. Not `portable-pty`. |
| VT emulator | `vt100 = { version = "=0.18.1", default-features = false }`. Its `Callbacks` hooks carry OSC 7501, the terminal queries and the kitty keyboard state. |
| Status parser | No second byte scanner. The vt100 parser (vte inside it) is the only parser of the output stream. Its state persists across `process()` calls, so a sequence split across reads is still seen whole. Shepherd parses only the OSC body. |
| Turn | Starts at 0. It goes up each time the status leaves `working`, and on entering `blocked` with a (state, kind) pair different from the current one. |
| Conditional input | Per-pane input lock for the whole call. The guard is checked under the pane's state lock in the same critical section as a non-blocking write of the committing bytes. |
| Wait | `Condvar` on the pane state (`status`, `turn`, `live`, pending send). |
| Child env | `env_clear()`, then only `HOME`, `PATH`, `USER`, `LANG`, `TERM=xterm-256color`, plus code-supplied extra env. |
| Process lifetime | Each pane is its own session and process group (`setsid`, so pgid = pid). Shepherd also tracks the pane's descendants by pid and start time (§11, issue #10). Removal sends SIGHUP and SIGTERM to the group and the descendants, then SIGSTOP and SIGKILL to all of them after 2 s, while the output is still read; the leader is always reaped and its status recorded. |
| Restart | Panes do not survive a Shepherd restart. `survives_restart = false`. |
| Selection | `OPENAB_SHEPHERD_BACKEND=native\|hc`. The default becomes `native` only if the real-Kiro probe passes; otherwise it stays `hc`. |

Technology stack (locked once this ADR is approved): Rust 2021, std threads and `std::sync` (no async runtime), `libc` 0.2, `vt100` =0.18.1, and the existing `serde_json` and `tiny_http`. No other new dependencies.

## 3. PTY crate: `libc`, not `portable-pty` or `nix`

**Options.**

- **portable-pty 0.9.0** (wezterm). Used by openab-pty. Read its source (measured, `cmdbuilder.rs` and `unix.rs`):
  - `CommandBuilder::new` seeds the env from the parent process.
  - `as_command()` calls `env_clear()` and then **always sets `SHELL`** from the env or the passwd entry.
  - A `cwd` that is not a directory **silently falls back to `$HOME`**.
  - Program lookup uses its own `search_path`.
  - It owns the `pre_exec` closure (setsid, TIOCSCTTY, signal reset, `close_random_fds`), so we cannot add steps.
  - Dependencies: anyhow, filedescriptor, nix, log, shell-words, serial2 and others.
- **nix `openpty`**. Correct, but nix is a large crate for three calls.
- **libc + std.** `libc::openpty`, then `std::process::Command` with stdio set from the slave and a small `pre_exec`.

**Chosen: libc + std.** The child env must be exactly the whitelist (§10). portable-pty injects `SHELL`, and its cwd fallback hides a caller error. We need about 40 lines of unsafe glue, and this crate keeps its dependencies minimal (two today). `std::process::Command` already gives us `env_clear`, a PATH search through the *child's* `PATH` once it is set explicitly, `current_dir`, SIGPIPE reset and the child signal-mask reset. `Child::wait` gives us reaping.

**Spawn procedure** (`src/native/pty.rs`):

1. Take the global `SPAWN_LOCK` (`static Mutex<()>`). It is held from `openpty` until `spawn` returns. Reason: `openpty` returns descriptors without `FD_CLOEXEC`. A concurrent `run` on another thread could fork in that window, and its child would inherit this pane's master. That would let one pane read another's terminal, and it would keep the slave open after Shepherd dies. Shepherd forks nowhere else after startup: `config.rs` runs `git` before any backend exists, and tiny_http does not fork. The lock covers only spawns in `pty.rs`: any future code that forks after startup must take it too (review N7).
2. Call `libc::openpty(&mut master, &mut slave, null, null, &winsize{rows: 120, cols: 200})`. Set `FD_CLOEXEC` on both descriptors. Set `O_NONBLOCK` on the master.
3. Build the `Command`:
   - `argv[0]` and its args, or `/bin/sh -c <line>`.
   - `env_clear()`, then `envs(whitelist)`.
   - `current_dir(cwd)`.
   - stdin, stdout and stderr each from `slave.try_clone()`. `dup2` onto 0, 1 and 2 clears CLOEXEC for those three only.
4. In `pre_exec`, call only async-signal-safe functions:
   - `setsid()`
   - `ioctl(0, TIOCSCTTY, 0)`
   - `signal(SIG_DFL)` for SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGCHLD and SIGALRM
   - `sigprocmask(SIG_SETMASK, empty)`
   - Any failure returns `Err(io::Error::last_os_error())`, and the spawn then fails.
5. Spawn, drop the parent's slave copies, release `SPAWN_LOCK`.

Every other descriptor Shepherd opens comes from std, which sets `O_CLOEXEC`, so no descriptor closing loop runs in the child. An integration test checks this (§14).

**Failures.**

- `openpty` or `fcntl` fails: `ErrorKind::Other`, detail `native: openpty failed: <errno text>`. Nothing is spawned or retained.
- `spawn` fails, for example ENOENT (program not found), EACCES, or a `pre_exec` failure: `ErrorKind::Other` (hc maps `spawn_failed` to `backend_error` as well), detail `native: spawn <argv0> failed: <io error>`. Both descriptors are closed and no pane is recorded.
- Both cases are logged once on stderr, at warn level: `openab-shepherd: native: spawn failed: <argv0>: <error>`. The env and the text are never logged.

## 4. VT emulator: `vt100` 0.18.1

**Criteria:** fidelity of the screen and scrollback for agent TUIs, hooks for OSC passthrough and terminal queries, maintenance, and size.

| | vt100 0.18.1 | alacritty_terminal 0.26.0 | wezterm-term |
|---|---|---|---|
| Screen and scrollback | Grid with a bounded scrollback (`Parser::new(rows, cols, scrollback)`), `set_scrollback(offset)` paging, wide characters (unicode-width). Tracks DECCKM (`application_cursor()`), bracketed paste (`bracketed_paste()`) and the alternate screen (47, 1049) (measured in source). It does not handle 1047 or 2026, which reach `unhandled_csi`. | Highest fidelity: reflow, many modes, kitty keyboard modes in `TermMode`. | High fidelity. |
| Hooks | `Callbacks` trait (measured): `unhandled_osc(screen, params)`, `unhandled_csi(screen, i1, i2, params, c)`, `on_scroll`, each called in stream order with `&mut Screen`, so the cursor position is exact at the query. | `EventListener` sends `PtyWrite` replies itself. An unknown OSC is dropped inside its `Handler`, so 7501 would need a second scanner. | Not on crates.io (git dependency). |
| Maintenance | Released 0.17.0, 0.18.0 and 0.18.1 in Oct 2026, MSRV 1.85 (measured on crates.io). One maintainer. | Active (Alacritty), but the API breaks between minors, and it brings its own tty and event loop (rustix-openpty, polling, signal-hook, parking_lot, regex-automata). | — |
| Deps | vte 0.15, unicode-width, itoa (plus arrayvec and memchr through vte without std). | Many. | Many. |

**Chosen: vt100 =0.18.1 with `default-features = false`.** Its callbacks put status, query answers and kitty tracking on the same parser that draws the screen. That rules out a status scanner and the screen disagreeing about where a sequence starts or ends (the rule from "Measurement Discipline": the check must use the same rule as the shipping code). It is synchronous, small, and needs nothing from an async runtime. alacritty_terminal is the fallback if fidelity proves inadequate. The trait boundary means swapping it touches only `src/native/term.rs`.

**Why without std (measured, vte 0.15.0 source):**

- With `std`, vte keeps the OSC body in an unbounded `Vec`. A child that writes `ESC ]` and then never terminates it would grow Shepherd's memory without limit.
- Without `std`, the buffer is an `ArrayVec<u8, 1024>`. Bytes past 1024 are dropped, and the sequence is still dispatched at its terminator.
- vt100's `std` feature only enables `vte/std` and `impl io::Write for Parser`. We call `process()` and do not need that impl.
- If the no-std build fails on remote build host (**open**, Q10), use the default features and put an OSC length guard in front of `process()`. The guard counts bytes since an unterminated `ESC ]` and feeds `CAN` (0x18) to abort the sequence at 1024.

**Pinning.** The version is pinned exactly (`=0.18.1`), because the set of sequences vt100 handles itself decides what reaches our callbacks. Bumping it requires re-running the callback unit tests in §14.

**Screen size (decided):** 120 rows by 200 columns. This is hc's default headless size (schema), the size at which the dialog layout was measured and the dialog parser written. No resize API exists in the trait, so the size is fixed for the pane's life.

## 5. OSC 7501 parsing

`Hooks::unhandled_osc(screen, params)` is the only entry point. vte splits the OSC body on `;` into at most 16 params.

1. If `params[0] != b"7501"`, the OSC is ignored (no reply, not recorded). vt100 handles 0, 1, 2 and 52 itself, and 10, 11 and 12 are handled in §8.
2. **Truncation check.** If `params.len() == 16`, or the sum of the param lengths plus the separators is 1023 or more, the OSC may have been cut by the bounds above. It is discarded and `bad_reports` goes up.
3. `body = params[1..].join(b";")`.
4. If the body is exactly `?`, this is the capability query. Reply `ESC ] 7501 ; ? : states=idle,working,done,blocked,error : kinds=permission,question,auth BEL`, written without the spaces. This is hc's reply string (measured), with BEL as explained in §8. Kiro 3.1.0 never sends the query; the reply exists for other programs that follow the protocol.
5. Otherwise split the body on `:` into fields, and each field at its first `=`.
   - Keys and values must be ASCII; a non-ASCII field is ignored.
   - A field with no `=` is ignored.
   - Unknown keys (for example `app`) are ignored, and the body is kept in the status log.
   - `state` is required. Without it, the report is discarded and `bad_reports` goes up.
   - The values of `state` and `kind` must be 1 to 32 bytes of `[a-z0-9_-]`; otherwise the report is discarded.
   - The first occurrence of a key wins.
6. The result is `ProgramStatus { state: State::parse(word), kind }`. `kind` is kept only when the state is `blocked`, which matches the trait's doc comment. It is passed to the status tracker (§6).

**Where buffering lives.** The vte parser keeps its state between `process()` calls. A report split at any byte, including inside the ESC-`]` pair or between ESC and `\` of an ST, is dispatched once, when its terminator arrives. CAN and SUB abort it, the same way a real terminal does. Both BEL and ST terminate it. The OSC body is bounded at 1024 bytes as above.

**Status log.** A per-pane ring of the last 64 accepted or discarded reports, each with `at_ms`, the raw body bytes (at most 1024), the parsed status or the reason it was discarded, and the turn after it. It is exposed only through an inherent method `NativeBackend::status_log(&TerminalRef)`, used by tests and the probe. It is not an MCP tool.

## 6. Status and turn semantics

Per pane: `status: Option<ProgramStatus>`, `reported: bool`, `turn: u64` (starts at 0), `status_ms: Option<u64>`.

When a report `new` arrives and the pane is live:

```
prev = status
leaves_working = prev.state == Working && new.state != Working
new_block      = new.state == Blocked && (prev.state, prev.kind) != (Blocked, new.kind)
if leaves_working || new_block { turn += 1 }
if (prev.state, prev.kind) != (new.state, new.kind) { status_ms = now_ms }
status = Some(new); reported = true
if new.state == Working { pending_send = None }
if turn changed { pending_send = None }
notify_all(changed)
```

Checked against the measured hc event streams:

- **Simple turn** (osc7501 probe): `idle`(0) → `working` → `done` gives turn 1. ✓
- **Two blocked prompts** (blocked probe): `idle`(0) → `working` → `blocked` (1) → `working` → `done` (2) → `working` → `blocked` (3) → `working` → `done` (4). ✓
- A repeated identical report does not change the turn or `status_ms`.

`new_block` adds two cases hc was never observed in:

- `blocked` arriving straight from `idle` or `done`.
- A blocked kind changing with no `working` in between.

Both go up so that `answer_dialog`'s `expect_turn` guard detects a different dialog. Whether hc does the same is unknown (**open**, Q5). Going up is the safe direction.

**When the program ends** (the leader is reaped, §11):

- `live = false` and `exit_code` is set.
- `status = None`, as the trait says ("null ... after it ended").
- `reported` and `turn` are kept.
- `notify_all`.

**Exit code:** `ExitStatus::code()` when the program exited normally, else `128 + signal`, the shell convention.

## 7. Key encoding

The keyboard mode is read at write time, under the state lock:

- `app_cursor = screen.application_cursor()` (DECCKM, from vt100)
- `kitty = kitty_flags[screen.alternate_screen() as usize]` (§8)

| Key | Legacy (kitty flags 0) | Kitty, flag 1 (disambiguate) set, flag 8 clear | Kitty, flag 8 (report all keys) set |
|---|---|---|---|
| `Enter` | `\r` | `\r` (the spec keeps Enter, Tab and Backspace legacy) | `ESC [ 13 u` |
| `Up` | `ESC [ A`; `ESC O A` when DECCKM is on | `ESC [ A` | `ESC [ A` |
| `Down` | `ESC [ B`; `ESC O B` when DECCKM is on | `ESC [ B` | `ESC [ B` |
| `Esc` | `\x1b` | `ESC [ 27 u` | `ESC [ 27 u` |
| `CtrlC` | `\x03` (the line discipline turns it into SIGINT for the foreground group when ISIG is on) | `ESC [ 99 ; 5 u` | `ESC [ 99 ; 5 u` |
| `Other(name)` | refused: `ErrorKind::Unsupported("key <name>")`, nothing written | same | same |

Source: kitty `docs/keyboard-protocol.rst` (master, fetched 2026-11-10, measured):

- With disambiguate, keys that produce no text use `CSI number ; modifier u` or `CSI 1 ; modifier [ABCD…]`. The `1` and the modifier are omitted when there are no modifiers.
- Ctrl is modifier bit 4, sent as 1 + 4 = 5. `c` is code point 99.
- Esc is 27 and Enter is 13.

**Other flags.**

- Flag 2 (report event types) needs no change: press is the default event type and is omitted. We never send release events. A program that waits for releases of these keys would see presses only (**open**, Q7).
- Flags 4 and 16 do not change these five keys.

**Arrows under DECCKM with kitty flags.** The spec lists only CSI forms once disambiguate is on, so the table uses `CSI A` and `CSI B` in that case. The probe confirms this on Kiro (**open**, Q7).

**Behavior on unknown keys.** `keys` checks every key before writing any byte. One unsupported key refuses the whole call, and nothing is written (rule: "Validate every case before mutating any").

## 8. Terminal queries the core answers

Answered in `Hooks::unhandled_csi` and `Hooks::unhandled_osc`, in stream order. Replies are appended to the pane's `pending_replies` buffer and written by the flush rule in §9. Parameters are compared numerically, so `CSI 0 c` and `CSI c` are the same query.

| Query | Bytes in | Reply (exact) | Note |
|---|---|---|---|
| DA1 | `CSI c`, `CSI 0 c` | `ESC [ ? 6 2 ; 2 2 c` | VT220 with ANSI colour. It claims nothing we do not emulate (no sixel, unlike openab-pty's SwiftTerm answer). |
| DA2 | `CSI > c`, `CSI > 0 c` | `ESC [ > 1 ; 1 0 ; 0 c` | VT220, firmware 10. It does not claim xterm (41). |
| DSR status | `CSI 5 n` | `ESC [ 0 n` | |
| CPR | `CSI 6 n` | `ESC [ <row+1> ; <col+1> R` | `screen.cursor_position()` at the moment of the callback, so it is exact in stream order. openab-pty cannot answer this one. |
| DECXCPR | `CSI ? 6 n` | `ESC [ ? <row+1> ; <col+1> R` | |
| Kitty query | `CSI ? u` | `ESC [ ? <flags> u` | The flags for the active screen. |
| OSC 10 / 11 / 12 query | `OSC 10 ; ?`, `OSC 11 ; ?`, `OSC 12 ; ?`, and multi-group forms (`OSC 10 ; ? ; ?` is foreground, then background) | `ESC ] 10 ; rgb:c7c7/c7c7/c7c7 BEL`, `ESC ] 11 ; rgb:1e1e/1e1e/1e1e BEL`, `ESC ] 12 ; rgb:c7c7/c7c7/c7c7 BEL` | The dark palette openab-pty declares; the cursor colour is reported as the foreground. Set forms are not answered and not mirrored. A sequence that mixes sets and queries gets no reply. |
| OSC 7501 query | `OSC 7501 ; ?` | see §5 | |

**OSC terminator (decided).** vt100's `unhandled_osc` does not pass on whether the query ended in BEL or ST (measured). xterm echoes the query's terminator. Kiro's queries end in BEL (measured), so every OSC reply here uses BEL. Whether Kiro also accepts a BEL reply to a query it ended in ST does not arise for Kiro 3.1.0 (**open**, Q8, in case another program needs ST).

**Kitty keyboard state (decided).**

- Two stacks, one per screen. The active one is `screen.alternate_screen()` read inside the callback, so it is the emulator's own screen state, not a second parse of `CSI ? 1049 h`.
- Each stack is bounded at 16 entries, with flags masked to `0x1f`:
  - `CSI > f u` pushes the current flags and sets `f`; at 16 entries the oldest is dropped.
  - `CSI < n u` pops `n` entries (default 1); popping past the bottom clears the stack and the flags.
  - `CSI = f ; m u`: mode 1 sets, 2 ORs, 3 subtracts; any other mode is ignored.
- These are the semantics openab-pty mirrors from SwiftTerm.
- **Known limit:** `ESC c` (RIS) is handled inside vt100 with no callback, so it does not clear the kitty stacks. The flags stay stale until the program sets them again.

**Not answered:** XTVERSION (`CSI > q`), DECRQM (`CSI ? Ps $ p`, including the 2026 support query), XTGETTCAP, DECRQSS, DA3, `OSC 4`, colour scheme DSR (`CSI ? 996 n`), and every DEC private DSR other than `?6`. A wrong answer claims a capability we do not have. Kiro runs without them (measured: XTVERSION was unanswered in the plain-PTY probe).

**Synchronized output.** `CSI ? 2026 h` and `CSI ? 2026 l` reach `unhandled_csi` and set or clear `sync_since: Option<Instant>`. `read` waits on the condvar while a synchronized update is open, for at most 150 ms, so it does not return half a frame (§9). A sync that stays open longer than 1 s is treated as closed.

## 9. Pane runtime, locking and conditional input

```
NativeBackend { panes: Mutex<Table>, config: NativeConfig, env_source }
Table  { next_id: u64, panes: Vec<Arc<Pane>>, exiting: bool } // creation order
Pane   { id, name, argv, cwd, pid,
         state: Mutex<PaneState>, changed: Condvar,
         input: Mutex<Input>,                         // the master fd for writing
         tree: Mutex<procs::Tree>,                    // descendants, §11
         stopping: AtomicBool, shutdown: AtomicBool, reader: JoinHandle, reaper: JoinHandle }
PaneState { vt: vt100::Parser<Hooks>, status, reported, turn, status_ms,
            live, exit_code, pending_send: Option<u64>, pending_replies: Vec<u8>,
            raw_ring: RingBuf(256 KiB), last_output: Instant, bad_reports, status_log }
```

**Lock order:** `Table`, then `Pane.input`, then `Pane.state`; `Pane.tree` is taken before `state` and never together with `input`. Code never takes `input` while it holds `state`. No lock is held while sleeping, polling or waiting, except the condvar wait on `state`, which releases it. The table lock is held only to look up, insert or evict, never across I/O.

**Reader thread** (one per pane). It loops:

- `poll(master, POLLIN, 200 ms)`; on each wake it checks `shutdown`. `shutdown` is set only after the leader has been reaped (§11), so the terminal is drained for as long as any pane process may still be exiting.
- Every 250 ms (`TREE_POLL`), while the leader runs, it refreshes the descendant tree with `tree.try_lock()` (a stop in progress holds the tree; the reader must not block on it, or it would stop draining).
- It reads up to 16 KiB.
- `read` returning 0 or `EIO` means the slave side has closed, and the thread exits its loop. `EAGAIN` or `EINTR` means retry. Any other error is logged once (`native: <id>: read failed: <errno>`) and ends the loop.
- For each chunk, under `state`:
  1. Append the chunk to the raw ring.
  2. `vt.process(chunk)`. The callbacks queue status reports, replies and sync changes.
  3. Apply the status reports in order (§6), set `last_output`, and `notify_all` when anything changed.
- After releasing `state`, it flushes `pending_replies` with `input.try_lock()`. When the lock is held, the holder flushes at its next boundary.
- `pending_replies` is capped at 4 KiB. Overflow drops the newest replies and is counted. This bounds a child that floods queries while not reading its input.

**Reaper thread** (one per pane). `waitid(WEXITED | WNOWAIT)` blocks until the leader exits without reaping it. Then, under `state`: `live = false`, set `exit_code`, `status = None`, `notify_all`. The leader stays an unreaped zombie, so its pid (the group and session id) stays reserved, while what it left behind is ended: during a removal or shutdown (`stopping`) the reaper waits for that stop to `release` the pane; on the program's own exit it runs the stop itself (§11). Then it reaps the leader under the tree lock and sets `reaped`. Every leader is reaped this way; none is kept as a `<defunct>` until removal (issue #10). See §11.

**Resolving a `TerminalRef`.** Ids have the form `n:<k>`, from a per-backend counter starting at 1 and never reused. `:` is outside the name alphabet, so an id can never equal a name. A ref that matches an id resolves to that pane. Otherwise it is matched as a name, live pane first, then the most recent finished pane with that name. No match gives `NotFound`.

**The guard check** (`check_guard(&PaneState, &InputGuard) -> Result<(), BackendError>`), in this order:

1. `!live`: `Finished`.
2. `refuse_when_blocked` and the state is `Blocked`: `Blocked { kind: status.kind.clone() }`. Unlike hc, the kind is included.
3. `if_state = Some(s)` and the state is not `s` (no status counts as a mismatch): `StatusMismatch`.
4. `if_kind = Some(k)` and the kind is not `Some(k)`: `StatusMismatch`.
5. `expect_turn = Some(t)` and `turn != t`: `StatusMismatch`.

**Guarded write** (`write_guarded(bytes, guard)`), called with `input` held:

1. Lock `state`, run `check_guard`, and try one non-blocking `write` of all `bytes` while still holding `state`. While `state` is held, the reader cannot apply new output, so no status change can land between the check and the bytes entering the PTY. This is the atomicity hc lacks for keys.
2. If the write took every byte, it is done. If it returned `EAGAIN` with **zero** bytes written, release `state`, `poll(POLLOUT)` for the remaining write deadline, and go back to step 1, re-checking the guard. Nothing has been written yet, so the re-check is still exact.
3. If the write was partial, the commit has started: release `state` and write the rest with `poll(POLLOUT)` and no further guard. The sequences here are at most 16 bytes for a key and a few KiB for a paste, so this is rare. It is documented.
4. The deadline is 5 s per call (`WRITE_TIMEOUT`). If nothing was written by then: `ErrorKind::Timeout`, detail `native: <id>: the program is not reading input; nothing was written`. If part was written: `ErrorKind::Other`, detail `native: <id>: input partly written before the program stopped reading`.

The reader keeps running while a writer waits for `POLLOUT`, because the writer holds `input`, not `state`. A child blocked on output therefore drains, and no deadlock can form between output and input.

**`send(t, text, guard)`:**

1. Validate `text` again in the backend: refuse any control character except TAB (`InvalidInput`), and any text over 4096 bytes. The tools already enforce this. The backend repeats it because bracketed-paste safety depends on it: no ESC, so no `ESC [ 201 ~` breakout.
2. Take `input`. Flush `pending_replies` (a boundary).
3. Under `state`: read the mode (bracketed paste from `screen.bracketed_paste()`) and record `turn_at_send` and `status_at_send`.
4. Paste through `write_guarded`. The paste is `ESC [ 200 ~ <text> ESC [ 201 ~` when bracketed paste is on, else `<text>`.
5. Settle: wait on the condvar until the pane has produced no output for 100 ms, for at most 1 s (**open**, Q11). This lets the program take in the paste before Enter.
6. Flush `pending_replies`. Then send Enter (§7) through `write_guarded` with the guard **plus** `expect_turn = Some(turn_at_send)`.
   - If that guard fails, Enter is not sent and the call returns `ErrorKind::Other`, detail `native: <id>: the text was pasted but Enter was not pressed: <reason>`.
   - It is deliberately not `Blocked`, because Shepherd's `pane_blocked` message says "nothing was typed", and that would be false here. The trait has no partial-input kind (**open**, Q14).
   - Pressing Enter anyway could select Allow once in a dialog that appeared during the paste, so it is never done.
7. Release `input`. Wait on the condvar up to 2 s (`START_WAIT`) for `state == Working` or `turn != turn_at_send`.
8. Return `SendResult { turn: Some(turn_at_send), status: status_at_send.map(state), started }`:
   - `started = None` if the program never reported a status.
   - `started = Some(true)` if it went working.
   - `started = Some(false)` otherwise; then also set `pending_send = Some(turn_at_send)` (see `wait`).

**`keys(t, keys, guard)`:**

1. Encode every key first (§7). Any failure refuses the call before any write.
2. Take `input` and flush replies.
3. Run one `write_guarded` for all keys: the guard is checked once, and the keys are encoded under the same `state` lock with the mode read there and written as one byte string. A refusal therefore means no key was written. `answer_dialog` passes `if_state = Blocked`, `if_kind = permission` and `expect_turn = T`, so its Enter is refused atomically if the dialog changed. (Review N4: checking per key could write the first keys and then report "nothing was written".)

`write_guarded` writes pending query replies first, and whole, in the same critical section: user bytes are written only once no reply is half-written (review N6b).
4. Return `KeysResult { status, turn }` as read under `state` after the last write.

**Capabilities.** `conditional_input` and `conditional_keys` are both true. Every guard field is enforced, and `refuse_when_blocked = false` is accepted.

## 10. Child environment

The env is built by `child_env(lookup: impl Fn(&str) -> Option<OsString>, extra: &[(OsString, OsString)])`. The lookup function is injected so tests can supply a fake parent env (the same pattern as `config::load_config`). Production passes `std::env::var_os`.

| Variable | Value |
|---|---|
| `HOME` | the parent's value if set; else not set |
| `PATH` | the parent's value if set; else `/usr/bin:/bin:/usr/sbin:/sbin` |
| `USER` | the parent's value if set; else not set |
| `LANG` | the parent's value if set; else `en_US.UTF-8` (the dialog parser needs UTF-8 output such as `❯`) |
| `TERM` | always `xterm-256color` (fixed; the parent's TERM is ignored) |
| extra | `NativeConfig.extra_env` (code only: tests, the probe), then the launch profile's `RunSpec.profile_env`, then the run's `RunSpec.env`. Validated: key `[A-Z_][A-Z0-9_]*`, value without NUL, at most 32 entries together. |

Nothing else passes, by construction (`env_clear()` first). In particular, `OPENAB_SHEPHERD_TOKEN`, `OPENAB_SHEPHERD_TOKEN_FILE`, `OPENAB_SHEPHERD_PROFILES`, `OPENAB_SHEPHERD_BACKEND`, `HC_*`, `KIRO_API_KEY`, `TERM_PROGRAM*`, `COLORTERM`, `SSH_*` and cloud credentials never pass by name.

**Launch profiles (owner decision for review SOL-B1, supersedes the Kiro launch path).** The first probe run showed kiro-cli needs `KIRO_API_KEY` (Q9). An earlier fix passed the parent's key to any `argv` run of the operator's `kiro-cli`; Sol's review showed that still gave a caller-chosen run (its arguments, cwd and env) a parent credential. That rule is removed. A credential now reaches a child only through an operator-defined launch profile (`src/profiles.rs`, README "Launch profiles"): a JSON file named by `OPENAB_SHEPHERD_PROFILES`, outside every git repository, where each profile fixes `argv` + `extra_args`, an allowlist of canonical working directories, and an `env` map whose values are `keychain:<service>` (read at launch with `/usr/bin/security find-generic-password -s <service> -w`, macOS only), `env:<NAME>` (Shepherd's own variable, at launch) or a non-secret literal. `start_pane` takes `profile` (plus `name` and a `cwd` the profile allows) and refuses caller `argv`/`line` unless `allow_arbitrary_command` is set; such a command never gets a profile's env, and a profile run takes no caller env. Shepherd resolves the profile, reads the values last, and passes them as `RunSpec.profile_env` (`Secret`, redacted `Debug`); the native backend adds them as extra variables, hc refuses a non-empty `profile_env`. The keychain read takes the native `SPAWN_LOCK` around its fork. Residual: a profile's program and what it runs (Kiro's tools) hold the credential; profiles limit which program gets it, not what that program does with it.

**Profile executable and file (second review, SOL2-B1, SOL2-B2, O-4).** A bare `argv[0]` in a profile used to reach `Command::new` unresolved, so the child's `PATH` lookup happened after `current_dir` switched to the caller's cwd: with `.` or an empty entry in Shepherd's `PATH`, an impostor in an allowed directory got the profile's credentials. Now `argv[0]` must be a name or an absolute path; a name is resolved when the pane starts, before any source is read, in Shepherd's `PATH` using only absolute entries (directory canonicalized, file name kept), and the run's `argv[0]` is that absolute path (no executable: `not_found`, nothing read). A profile's `env` may not set `PATH`, `DYLD_*` or `LD_*` either. The profiles file must be a regular file owned by Shepherd's effective uid with no group or other write bit; it is checked with `fstat` on the descriptor that is then read. Residual: an absolute `PATH` directory writable by someone else is the operator's to avoid.

**Per-run env (review B2, resolves Q4).** `RunSpec.env: Vec<(String, String)>` carries a run's own variables; `start_pane` takes them as an `env` object (at most 32, names `[A-Z_][A-Z0-9_]*`, values under the input rules, empty allowed), only for a caller-chosen command. A new capability `client_env` says whether a backend accepts it: native true, hc false (hc refuses a non-empty `env` as unsupported, and the tool refuses it before calling). They apply to that pane only. Protected names: a run may not set `HOME`, `PATH`, `USER`, `LANG`, `TERM` (the fixed baseline) or any `DYLD_*`/`LD_*` name; these give `InvalidInput` naming the key, never the value. Whether Kiro needs anything beyond this list (`TMPDIR`, `LOGNAME`, `SHELL`, `LC_*`) was settled by the probe: only `KIRO_API_KEY`, which now comes from a profile.

**cwd.** If given, it must be an absolute path to an existing directory; otherwise `InvalidInput`, detail `native: cwd must be an absolute existing directory`. There is no silent fallback. If omitted, it is the parent's `HOME`, or `/` if `HOME` is unset. `TerminalInfo.cwd` reports the directory used.

**argv.** 1 to 256 elements; otherwise `InvalidInput`. The tools already bound the size of each element.

## 11. Process groups, removal and shutdown

- `setsid` in `pre_exec` makes the child a session and group leader (pgid = pid) with the PTY as its controlling terminal.
- **Removal** is `TerminalBackend::remove(&TerminalRef, grace: Option<Duration>) -> Result<RemoveResult, BackendError>`, reached through the `remove_pane` tool (Q3, resolved). It runs:
  1. Note whether the pane is live, and set `stopping` (not `shutdown`: the reader keeps draining).
  2. Soft stop: refresh the descendant tree, then `kill(-pgid, SIGHUP)` and `kill(-pgid, SIGTERM)` if the group id is still ours, and SIGHUP and SIGTERM to every descendant. No key is sent first: a program can ignore Esc or Ctrl-C or sit in a dialog, while the signals reach every process.
  3. Wait up to `grace` (default 2 s, `KILL_GRACE`; `remove_pane` takes 0 to 60 s) for the leader to exit (`exited`, set by the reaper as soon as `waitid` returns, before it waits up to 500 ms for the reader to drain and clears `live`). Not exited at the end means `stopped: killed`, else `exited`, so `killed` always comes with 128 + SIGKILL (review m3). In the rest of the same grace, wait for the descendants to exit (polled every 50 ms, refreshing the tree).
  4. Release (hard stop, below): SIGSTOP and SIGKILL to the group and every descendant left.
  5. Wait up to 2 s for the reaper to record the exit code and reap the leader. A killed leader reports 137.
  6. Set `shutdown` and join the reader. The reader ends on EIO or after its next 200 ms poll. Remove the pane from the table; the master closes when the last handle to the pane drops.
- **Hard stop (`release`).** Under the tree lock: refresh the tree, SIGSTOP the group and every descendant, and repeat (at most 8 rounds) until a refresh finds nobody new. A stopped process cannot fork, so this closes the window between the last look and the kill. Then SIGKILL the group (under `state`, only while the group id is ours) and every descendant, and set `released`. Idempotent: eviction, replacement by name and the reaper may all call it. ESRCH counts as success.
- **Removal failures:** `NotFound` for an unknown ref. If `kill` on the group fails with an error other than ESRCH, it is logged at warn level and removal still completes; EPERM after the leader has exited is not logged, because macOS answers EPERM for a group left with only its zombie leader (measured).
- **Descendant tree (issue #10).** Measured on macOS with main `b446018`: kiro-cli starts `bun`, which starts `node …/acp-server.js` in a new session (`ps` STAT `Ss` without `+`: it called `setsid`, so it is a session and group leader with no controlling terminal; Node's `detached: true`). It is found only by the parent-pid walk from `bun`, never through the pane's group or terminal. Killing the pane's group never reached it, and after a pane ended it lived on with ppid 1 for over an hour. `procs::Tree` (in `src/native/procs.rs`) records every descendant as `(pid, start time)`:
  - **Found by** parent pid, from the leader and from every member still alive (a breadth-first walk), and by membership of the leader's group and session: on Linux, `/proc/<pid>/stat` fields `ppid`, `pgrp`, `session` and `starttime`, from one scan of `/proc`; on macOS, `proc_listchildpids`, `proc_listpgrppids`, and `proc_listpids(PROC_TTY_ONLY, tdev)` with the leader's controlling terminal (`proc_bsdinfo.e_tdev`; macOS has no list-by-session call, and every process of the session has that terminal while the leader runs), with the start time from `proc_pidinfo(PROC_PIDTBSDINFO)` (`pbi_start_tvsec`/`usec`).
  - **Refreshed** every 250 ms by the reader while the leader runs, and at the start of every soft and hard stop. A member is kept across refreshes while its pid still has the recorded start time, so a descendant whose parent exits (reparented to pid 1) stays known. Group and session lookups by the leader's pid run only while that pid is reserved (the leader is unreaped), and the reaper reaps under the tree lock, so no lookup can run on a freed pid.
  - **Signalled** only after re-reading the pid's start time and finding it unchanged. A reused pid has a later start time and is skipped. The check and the `kill` are a few microseconds apart; a pid is reused only after the pid space wraps, so a reuse inside that window is not a practical risk. Zombies are skipped (Linux state `Z`/`X`; macOS does not answer `proc_pidinfo` for zombies). Shepherd's own pid and pids 0 and 1 are never members.
  - **Known limit (owner decision, review B1):** a descendant that is re-parented and fully detaches before the next tree poll (about 250 ms) is not seen: it calls `setsid` (no group, session or terminal link to the pane) and every ancestor between it and a known member exits first, as in a daemon's double fork. It survives removal, the program's own exit, shutdown and drop. One that only changes its process group is still found through the session (macOS: the terminal) while the leader runs, and one whose parent lives until the next poll is found by the parent-pid walk. Measured on macOS 15.7 (remote build host, a C probe with a `setsid` double fork): kqueue `EVFILT_PROC` with `NOTE_TRACK` is refused with `EV_ERROR`, data 45 (`ENOTSUP`); every process inherits its parent's coalition ids (`PROC_PIDCOALITIONINFO`), but creating a coalition needs root, so a pane cannot get its own; macOS has no subreaper and no cgroup; `p_puniqueid` (`PROC_PIDUNIQIDENTIFIERINFO`) survives the reparent to pid 1, but it names the intermediate that was never seen, so the chain stays broken. No public unprivileged macOS mechanism closes the gap. Linux can close it with `PR_SET_CHILD_SUBREAPER` in a per-pane supervisor; that is a follow-up to build and test with a Linux build.
  - **Alternatives considered.** (a) Signal only the group or session: misses `setsid` children, and neither kernel has kill-by-session. (b) Snapshot the tree only at stop time: misses a descendant whose parent already exited (the measured orphan), so tracking is required. (c) Linux `PR_SET_CHILD_SUBREAPER` would reparent orphans to Shepherd, but macOS has no equivalent and Shepherd would then have to reap foreign children; one mechanism on both platforms is simpler to test. (d) A per-pane cgroup (Linux) or job object does not exist on macOS. (e) pidfds would close the check-to-kill window on Linux only; not worth a second code path given (c).
- **The zombie and the lost exit code (issue #10).** Measured: `remove_pane` on Kiro returned `stopped: killed, exit_code: null`, and the leader stayed `?Es` (exiting, not yet a zombie) with Shepherd as parent. Cause: removal set `shutdown` first, so the reader stopped reading the master. A process that exits with output still queued on its terminal waits in the tty close for it to drain; with the master open and unread, the leader stayed in that state (`?Es`, measured on macOS) and never finished exiting, so `waitid` never returned and no code was recorded. The same unread output is why Kiro, which writes as it restores the terminal, did not exit within the 2 s grace. Fix: the reader drains until the leader is reaped (`shutdown` is set after), and every stop path waits for the reaper. Reproduced in `cargo test` before the fix with `trap '' HUP TERM; exec yes` (killed, `exit_code: None`, `ps` showed `?Es`) and with a shell that writes 400 KB on SIGHUP (killed instead of exiting with its code).
- **Graceful stop for Kiro (decided).** SIGHUP and SIGTERM with the output drained, no keystrokes. Typing `/quit` would need Kiro to be idle and not in a dialog, and would put text into a pane the caller may not expect; Ctrl-D/Esc have the same problem (`interrupt_pane` exists for a clean turn end). Measured with `examples/native_leak_check` on the laptop (kiro-cli 3.1.0): with the drain fix kiro-cli exited on the signals in 148 to 269 ms with code 1 in 4 of 5 runs (one of them with a 15 s grace); in the other it had not exited after 2 s and was killed (137, reaped, none of its processes left). Callers who want more time pass `remove_pane`'s `timeout`.
- **Pgid reuse policy (review B3).** Group cleanup is tracked separately from leader liveness. The reaper thread waits with `waitid(P_PID, pid, WEXITED | WNOWAIT)`: it reads the exit status but leaves the leader a zombie. POSIX does not reuse a pid while that process exists, so the group and session ids stay ours for as long as the zombie does. Shepherd signals a group only while it holds that reservation (`!reaped && !group_lost`), and the reaper reaps only after `release` has sent the final SIGKILL (sent under `state`). Leader exit and PTY EOF are not taken as proof that the group is empty. If `waitid` ever fails, the reaper first marks the group lost under `state` (`group_lost`, which `owns_group` and `release` check), then falls back to a plain wait, so the group is not signalled again (review N9); descendants are still signalled by their recorded start times. The reaper takes the `Child` from a shared slot when its thread starts; if the thread cannot start, `insert` kills the group, reaps the leader itself and returns an error, so no zombie is left (review SOL-B2). `end_all` sets `exiting` under the table lock, and a child spawned after that is ended in `insert` instead of entering the table (review N12). Shepherd's `main` also ends the panes when the HTTP server returns an error or the main thread panics (review N11).
- **Leader exit while other processes of the pane live.** The kernel sends SIGHUP to the foreground group when the controlling process exits. The reaper then gives what is left (group members and descendants) the soft stop and up to 2 s, then the hard stop, and reaps the leader. The pane shows as finished from the moment the leader exits. (Before issue #10 the leftovers lived, and the leader stayed `<defunct>`, until the pane was removed, evicted or replaced.)
- **Shepherd shutdown.**
  - `NativeBackend::shutdown` (also run when the last handle drops) sets `stopping` and runs the soft stop on every pane, waits once, up to 2 s total, for the leaders and then their descendants, runs the hard stop on every pane, and waits up to 2 s more for every leader to be reaped before stopping the readers.
  - `main` calls `NativeBackend::end_panes_on_exit_signals` before any other thread starts: SIGINT, SIGTERM and SIGHUP are blocked in every thread and taken by one `sigwait` thread, which runs `shutdown` and exits with 128 + the signal (review N1). Children unblock all signals in `pre_exec`.
  - If Shepherd is killed with SIGKILL, so that neither runs, its master descriptors close. The slave hangs up, and the kernel sends SIGHUP to each session's controlling process. Kiro exits on it. This is why the master must not leak into other children (`SPAWN_LOCK`, §3).
- **Capacity.**
  - At most 8 live panes (`MAX_LIVE_PANES`); a `run` beyond that gives `ErrorKind::Other`, detail `native: 8 panes are running; the limit is 8`.
  - At most 32 finished panes are retained (`MAX_FINISHED_PANES`), and the oldest finished one is evicted first. Eviction runs the hard stop (normally a no-op: the reaper already ended a finished pane's processes).
  - A `run` whose name matches a finished pane evicts that finished pane, the same way. Names are unique among **live** panes, which is hc's rule (schema: "unique among live terminals"). A taken name gives `NameTaken`.
- **Restart.** Panes live in memory and die with Shepherd. `survives_restart = false`. This is documented in the README and MVP-REPORT known limits.

## 12. The remaining trait methods

- **`get`** returns the `TerminalInfo` built under `state`.
- **`list(all)`** returns panes in creation order. With `all = false`, finished panes are left out and `hidden_finished = Some(count)`.
- **`read(t, n)`** waits up to 150 ms for an open synchronized update to close. Then, under `state`:
  - `n == 0`: the visible screen rows. Trailing blanks are trimmed per row and trailing empty rows are dropped. `source = Screen`, `complete = Some(true)`.
  - `n > 0`: the last `min(n, scrollback_len)` scrollback rows followed by the screen, read by paging `set_scrollback(offset)` from the top down. `set_scrollback(0)` is restored before `state` is released. `source = History`. `complete = Some(false)` when `n > scrollback_len` and rows have been evicted (counted in `on_scroll`), else `Some(true)`.
  - On the alternate screen there is no scrollback: the screen is returned, with `complete = Some(n == 0)` and `why = Some("alternate_screen")`.
  - Finished panes can be read.
  - The text contains no escape sequences.
- **`wait(t, timeout)`**: under `state`, loop:
  - `!live`: `{ended: true, reason: Exit, exit_code, turn}`.
  - `pending_send == Some(ts)` and the turn is still `ts` and the state is not `Working`: keep waiting. This is the turn the unstarted send began; hc follows the same turn, per the schema.
  - `reported` and the state is not `Working`: `{ended: true, reason: Status, status (with kind), turn}`. When the program is already not working, this returns at once, which is hc's documented behavior.
  - Otherwise `changed.wait_timeout(remaining)`. At the deadline: `{ended: false, reason: Timeout, status, turn}`.
  - A program that never reports a status counts as working until it exits.
- **`interrupt(t, timeout)`**: timeout defaults to 10 s, hc's default.
  1. `key = Esc` if `reported`, else `CtrlC`. It is encoded per §7 and written unguarded under `input`, as hc does, including when the pane is blocked (**open**, Q13).
  2. Record `fg0 = tcgetpgrp(master)` first.
  3. Outcomes:
     - The program ended: `Exited`.
     - It reported a status and the state is not `Working`, or the turn went up: `Stopped`, immediately if it was not working to begin with.
     - It reported a status and is still working at the deadline: `StillWorking`.
     - It never reported a status, and the foreground group went from `fg0 != pid` back to `pid` (a shell got its prompt back): `Stopped`.
     - Otherwise: `Unknown`.
  4. A finished pane gives `Finished`.
  5. `tcgetpgrp` on a macOS master is assumed to work as on Linux (**open**, Q12).

**Error mapping, by condition:**

| Condition | Kind | Recoverable |
|---|---|---|
| Unknown ref | `NotFound` | yes (caller fixes the name) |
| Name already live | `NameTaken` | yes |
| Pane finished (send, keys, interrupt) | `Finished` | no, for that pane |
| Guard: blocked | `Blocked { kind }` | yes (answer the dialog) |
| Guard: state, kind or turn mismatch | `StatusMismatch` | yes (re-read) |
| Bad text, cwd, argv or extra env | `InvalidInput` | yes |
| Unknown key | `Unsupported` | no |
| Write deadline passed with nothing written | `Timeout` | yes |
| Partial input, spawn or openpty failure, capacity reached, poisoned pane mutex | `Other` (detail says which) | depends; the detail names it |

Detail strings start with `native:` and never contain env values or the text that was input. A poisoned pane mutex means a callback panicked mid-update. Calls on that pane return `Other` with detail `native: <id>: pane state is inconsistent after a panic`. They do not unwrap the poisoned state, because it may be half-updated.

**Logging** goes to stderr with the `openab-shepherd: native:` prefix, at warn level only:

- spawn failure
- a reader error other than EOF or EIO
- a kill error other than ESRCH
- reply-buffer overflow, once per pane

Nothing is logged per chunk or per call.

## 13. Capabilities, selection and wiring

`Capabilities` at 9022867 has `conditional_input`, `conditional_keys`, `client_command`, `client_cwd` and `screen_text`. The task asks for full capabilities, including `program_status`, `history`, `native_wait`, `client_argv` and `survives_restart`.

**Decided:**

- The native PR adds `program_status`, `history`, `native_wait` and `survives_restart` to `Capabilities`, unless the merged trait already has them.
- `client_argv` is the existing `client_command`. It is not renamed, to avoid churn in the hc code and the tests.
- Flag values:

  | | hc | native |
  |---|---|---|
  | `program_status` | true | true |
  | `history` | true | true |
  | `native_wait` | true | true |
  | `survives_restart` | true | false |
  | `client_env` (added by review B2) | false | true |
  | All five existing flags | as today (`conditional_keys` false) | true |

- One tool change: `read_output` with `lines > 0` returns `unsupported` when `history` is false. The other new flags are informational for now. The README capability table gains these rows.
- Confirming the names with the trait owner is **open** (Q2).

**Selection (decided):** `OPENAB_SHEPHERD_BACKEND`, read in `config::load_config`.

- `native`: no `HC_SOCKET` needed; it is ignored if set.
- `hc`: `HC_SOCKET` is required, as today.
- Unset: the compiled default (`DEFAULT_BACKEND`). It becomes `native` only if the real-Kiro verification in §14 passes; otherwise it stays `hc`. The probe passed on 2026-11-10, so it is `native`.
- Any other value: refuse to start with exit 2, message `OPENAB_SHEPHERD_BACKEND must be native or hc`.
- `Config` gains `backend: BackendChoice { Hc { socket }, Native }`. `main.rs` builds `Shepherd::new(NativeBackend::new(NativeConfig::default()))` or `Shepherd::new(HcBackend::new(socket))`. The startup line names the backend. The env var name never reaches a child (§10).

**Crate layout (decided).** The probe must link the backend, so the crate gains `src/lib.rs`. It holds `pub mod` for `ansi`, `backend`, `config`, `dialog`, `hc`, `http`, `input`, `mcp`, `native`, `panelock` and `tools`, plus `#[cfg(test)] mod testutil`. `main.rs` keeps only the argument parsing and startup, using `openab_shepherd::…`. No behavior changes. New files:

- `src/native/mod.rs`: `NativeBackend`, the table, ref resolution, bounds, and the trait impl.
- `src/native/pty.rs`: openpty, spawn, `child_env`, poll read and write, `killpg`, `tcgetpgrp`.
- `src/native/term.rs`: the `vt100::Parser<Hooks>` wrapper, the query replies, kitty stacks, sync tracking and screen and history reads.
- `src/native/status.rs`: the OSC 7501 body parser and the turn tracker (pure; no I/O).
- `src/native/keys.rs`: `KeyboardMode` and `encode(key, mode)` (pure).
- `src/native/tests.rs`: integration tests with real programs.
- `examples/native_probe.rs`: the real-Kiro probe.

**Bounds and defaults** (constants in `src/native/mod.rs`, documented in the README):

| Bound | Default |
|---|---|
| Live panes | 8 |
| Retained finished panes | 32 |
| Screen | 120 rows × 200 cols |
| Scrollback | 2,000 lines, about 12.8 MB per pane at 200 cols (vt100 cells are 32 bytes, measured) |
| Read chunk | 16 KiB |
| Raw output ring | 256 KiB per pane (diagnostics and the probe only) |
| OSC body | 1,024 bytes (vte without std) |
| Status log | 64 entries |
| Pending replies | 4 KiB |
| State and kind words | 32 bytes |
| argv | 256 elements |
| Extra env (per run plus code-set) | 32 entries |
| Write deadline | 5 s |
| Paste settle | 100 ms quiet, at most 1 s |
| Start wait | 2 s |
| Read sync wait | 150 ms |
| Kill grace | 2 s |
| Interrupt default | 10 s |

Worst case: 8 live panes and 32 retained finished panes all keep their emulator with full scrollback (about 12.8 MB each) and a 256 KiB raw ring, so roughly 40 × 13 MB ≈ 520 MB, plus small per-pane state. 8 live panes alone are about 110 MB. (Review N2: the earlier figure counted only live panes.)


### Reconciliation with the merged trait

The trait merged in #5 (`main` at `091aac3`) differs from `9022867` as follows. Each is handled in the implementation as stated; where this ADR's earlier text disagrees, this list wins.

1. **`ScreenText.why` became `limit: Option<ReadLimit { Blocked, Other }>`.** The alternate-screen read (§12) sets `limit = Some(ReadLimit::Other)` instead of `why = "alternate_screen"`.
2. **`Key::Other` carries no name.** Native refuses it with `ErrorKind::Unsupported("sending an unknown key")`, detail `native: not supported: sending an unknown key`, the same wording as hc (§7's `key <name>` is superseded).
3. **`WaitResult.reason` is `Option<WaitReason>`.** Native always sets `Some`.
4. **`State::Other`, `ReadSource::Other`, `WaitReason::Other` and `InterruptOutcome::Other` are unit variants.** Native maps unknown words to them and keeps the raw word only internally.
5. **Kinds leaving a backend must pass through `backend::dialog_kind`.** Native keeps the raw kind internally (turn comparison, status log) and exports `dialog_kind(raw)`, so an unknown kind reaches Shepherd as `other`.
6. **`tools::resolve` requires the pane `get` returns to carry exactly the caller's name, and `start_pane` always sets a name.** Native ids `n:<k>` contain `:`, which is outside the name alphabet, so an id never equals a valid name (§9 already relies on this).
7. **`tools::start_pane` maps `run`'s `ErrorKind::NotFound` to "the program or working directory was not found".** Native therefore maps a spawn ENOENT and an absolute-but-missing cwd to `NotFound` (§3 and §10 said `Other` and `InvalidInput`). A relative cwd, or a cwd that exists but is not a directory, stays `InvalidInput`.
8. **`Capabilities` still has only the five flags, and the trait still has no `remove`** (since added; see Q3). The decisions above apply unchanged: the four flags `program_status`, `history`, `native_wait` and `survives_restart` are added, and `remove` is an inherent method on `NativeBackend`.

## 14. Testability and verification

**Unit tests (pure, no PTY).**

- `status.rs`:
  - A whole report.
  - The report fed to a `term::Emulator` split at **every** byte boundary, including inside `ESC ]` and between ESC and `\` of an ST. Each split produces exactly one status.
  - Both terminators.
  - Unknown keys and the first-key-wins rule.
  - A missing `state`, a bad word, non-ASCII bytes, an empty body, `7501;?` (gets the reply), a 7502 OSC (ignored).
  - Truncation (an OSC over 1,024 bytes, 16 params) is discarded.
  - Garbage: random bytes, a lone ESC, CAN in mid-report.
  - Turn rules against both measured streams in §6, repeated reports, blocked from done, a kind change, and exit (status cleared, turn kept).
- `term.rs`:
  - Every reply in §8, byte for byte, including CPR after cursor moves and a split query.
  - Kitty push, pop and set per screen, the 16-entry bound, `?u` after a switch to the alternate screen.
  - Mixed OSC set and query gets no reply.
  - 2026 tracking.
  - History paging restores the offset; `complete` after eviction.
- `keys.rs`: the full table in §7 for flags 0, 1, 1|2, 8 and 31, with DECCKM on and off; `Other` refused.
- Guard: every `check_guard` branch. `keys` with one unsupported key writes nothing (a fake writer).
- `child_env`:
  - The injected parent env holds `OPENAB_SHEPHERD_TOKEN`, `OPENAB_SHEPHERD_TOKEN_FILE`, `HC_SOCKET`, `AWS_SECRET_ACCESS_KEY` and `TERM=xterm-ghostty`. The output is exactly the whitelist, with `TERM=xterm-256color`.
  - Defaults when variables are absent.
  - Extra env validation.
- Bounds: the live-pane cap, finished-pane eviction, name reuse after finish, the reply-buffer cap, argv > 256.

**Integration tests** (`cargo test`, real processes, NativeBackend called directly with no HTTP and no listener):

- `sh -c 'echo hello'`: the screen shows `hello`, `wait` returns `Exit` with code 0.
- `cat`: a `send` echo round-trip and the bracketed-paste markers. A `printf '\033[?2004h'; cat -v` program shows `^[[200~` around the text.
- A scripted OSC program written with `printf`: `working`, `blocked` with kind `permission`, then `working` and `done`, with sleeps between. Assertions:
  - `get` and `wait` see each state, and the turns are 0 → 1 → 2.
  - `send` with `refuse_when_blocked` while blocked returns `Blocked{kind: permission}` and the program reads nothing (checked with a marker it would echo).
  - `keys` with a stale `expect_turn` returns `StatusMismatch` and nothing is written.
- Env: `/usr/bin/env` under a backend built with the injected lookup holding a fake token. The screen has none of the token variables, and exactly the whitelist keys appear.
- Descriptors: `sh -c 'ls /dev/fd'`, also from 8 threads at once, lists only 0, 1, 2 and ls's own directory descriptor.
- Interrupt: `sh -c 'sleep 30'` gets `CtrlC` with outcome `Exited` in under 10 s.
- Removal: `sh -c 'sleep 300 & sleep 300 & wait'`. After `remove`, `kill(-pgid, 0)` returns ESRCH within 3 s and the pane is gone from `list(true)`.
- Drop: dropping the backend kills a live group.
- Issue #10 (each failed on `b446018` before the fix):
  - `trap '' HUP TERM; exec yes`, removed with a 0.5 s grace: `stopped: killed`, `exit_code` 137, and the leader is reaped (`kill(pid, 0)` gives ESRCH) when `remove` returns.
  - A shell that writes 400 KB and exits 7 on SIGHUP: `stopped: exited`, `exit_code` 7.
  - A grandchild started with perl `POSIX::setpgid(0, 0)` or `POSIX::setsid()`, stdio closed: none survives `remove`, the program's own exit (with the leader reaped), `shutdown`, or dropping the last handle. A `setpgid` grandchild whose parent exited at once (reparented to pid 1) does not survive `remove`.
  - A group member that ignores HUP and TERM after its leader exits gets the grace, is then killed, and the leader is reaped without any removal.

**Real-Kiro probe** (`examples/native_probe.rs`, built on remote build host, copied back with `scp`, run on the laptop):

- **Arguments:** `--keep-dir` only. It never accepts `--host` or `--port`, and it opens no listener. It drives `tools::call` on a `Shepherd` over `NativeBackend`, so the hc-path safety checks (`pane_blocked`, `answer_dialog`) run unchanged.
- **Setup:** creates `/tmp/shepherd-native-probe-<pid>/.kiro/agents/probe.json` containing `{"name":"probe","tools":["@builtin"],"allowedTools":[],"toolsSettings":{}}`. Starts `kiro-cli chat --agent probe --trust-workspace` with that directory as cwd.
- **Steps:**
  1. Wait for `idle`. If no status arrives within 60 s, print the screen and fail; it never presses keys blindly.
  2. Prompt 1: "Reply with only the word OK. Do not use any tools." Wait for `done`.
  3. Prompt 2: "Use the shell tool to run exactly this command in the current directory and nothing else: touch shepherd_native_marker.txt". Wait for `blocked` with kind `permission`.
  4. Print the raw blocked OSC bytes in hex from `status_log`, and the kitty flags and DECCKM state.
  5. `send_input` must refuse with `pane_blocked`.
  6. Read the screen and parse it with `dialog::parse`.
  7. `answer_dialog deny_once`, then wait for `done`. The marker file must be absent.
  8. Prompt 3: a retry of the same `touch`. Wait for `blocked`, then `answer_dialog allow_once`, then wait for `done`. The marker file must be present.
- **Output:** every status transition, with time, raw body, state, kind and turn. The expected turns are 0 → 1 → 2 → 3 → 4 → 5.
- **Exit:** non-zero on any deviation.
- **Budget:** 3 Kiro prompts (the limit is 5).
- **Cleanup:** `remove`, then delete the directory unless `--keep-dir` was given.

**Real-Kiro leak check** (`examples/native_leak_check.rs`, issue #10; built on remote build host, copied back with `scp`, run on the laptop, then deleted). It takes only `--grace <s>`, opens no listener and sends no prompt. It starts a launch profile (`kiro-cli chat --trust-workspace`, `KIRO_API_KEY` from `keychain:openab-shepherd-kiro-api-key`) in `/tmp/shepherd-leak-check-<pid>`, waits for `idle` and for an `acp-server.js` whose cwd is that directory, records the pane's processes from `ps`, calls `remove_pane`, and fails if any of those processes survives, if any `kiro-cli` or `acp-server.js` still has its cwd in the directory, if the check has any child (zombie or not) left, or if `stopped`/`exit_code` break the rule (killed means 137).
- **Builds:** all builds and tests run on remote build host (`cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo build --release --all-targets`). The laptop never runs cargo or rustc.

## 15. Open questions

1. **Raw blocked bytes.** Is it `state=blocked:kind=permission:app=kiro-cli`, or are there other fields, or another order? The parser accepts any order and ignores unknown keys. The probe records the bytes. If the field is not named `kind`, the parser must learn the real name before the default switches.
   Resolved (probe, 2026-11-10): exactly `1b 5d 37 35 30 31 3b` + `state=blocked:kind=permission:app=kiro-cli` + `07`, i.e. `\x1b]7501;state=blocked:kind=permission:app=kiro-cli\x07`, BEL-terminated, the same both times. Every other report is `state=<word>:app=kiro-cli`. The field is named `kind`; no parser change.
2. **Capabilities fields.** Does the merged trait already include `program_status`, `history`, `native_wait` and `survives_restart`, and under which names? This design adds them if absent (§13).
3. **No `remove` in the trait and no MCP removal tool.** Native panes end only when their program exits, by `interrupt` or by the program's own quit, or when Shepherd exits. With 8 live slots this can fill up. Proposal: a follow-up adding `TerminalBackend::remove` and a `remove_pane` tool (hc: `terminal.kill` + `terminal.rm`). It is out of scope here unless the owner wants it in this PR.
   Resolved (follow-up): `TerminalBackend::remove` and the `remove_pane` tool (§11; hc: `terminal.get`, `terminal.kill` when live, `terminal.rm`). `list_panes` reports `live_panes` and `live_pane_limit` (`TerminalList.live_limit`, 8 here, none for hc).
4. **No `RunSpec.env`.** Per-run env exists only as code-set `NativeConfig.extra_env`. Should the trait gain a validated `env` field?
   Resolved (review B2): yes. `RunSpec.env`, the `client_env` capability and `start_pane`'s `env` (§10).
5. **Turn parity with hc** for `blocked` entered without `working`, and for a kind change while blocked. This design increments in both cases (§6).
6. **Wait after an unstarted send.** hc returns reason `not_started` only for `send` with `wait`, and a plain wait "follows that send's turn". This design follows the turn and never reports `not_started`. Confirm that is the hc parity the tools need.
7. **Kitty encoding on Kiro.** Which flags does Kiro push once `CSI ? u` is answered? And are `CSI A`/`CSI B` right under DECCKM with disambiguate on? The spec says CSI forms. The probe records the flags, and the dialog navigation in the probe exercises the result. Also: does any program need release events (flag 2)?
   Resolved (probe, 2026-11-10): kiro-cli 3.1.0 pushes kitty flags 1 (disambiguate) on the active screen and leaves DECCKM off, so Up/Down go as `CSI A`/`CSI B` and Enter as `\r`. `answer_dialog deny_once` moved the cursor with one Down and selected; `allow_once` needed no move. Flag 2 was not requested. Release events stay unanswered.
8. **OSC reply terminator.** Every reply uses BEL because vt100 hides the query's terminator. Does any program we care about require ST?
9. **Env sufficiency for Kiro.** Does kiro-cli need `TMPDIR`, `LOGNAME`, `SHELL` or `LC_*` (for example for auth or its keychain) beyond the whitelist? The probe settles it. Additions are by name only.
   Resolved (probe, 2026-11-10): `TMPDIR`, `LOGNAME`, `SHELL`, `SSH_AUTH_SOCK`, `__CF_USER_TEXT_ENCODING` and `XPC_*` are not needed (`env -i` with the whitelist plus all of them still gives "Not logged in"; the parent had no `LC_*`). kiro-cli needs `KIRO_API_KEY` when it has no stored login: without it the first probe run stopped at "You are not logged in" before any prompt. `child_env` passes `KIRO_API_KEY` by name when the parent has it, and (review B1) only on the Kiro launch path, an `argv` run of a program named `kiro-cli` (§10). Superseded (owner decision for SOL-B1): no variable passes by name; a launch profile's `env` supplies `KIRO_API_KEY` (§10).
10. **vt100 without std.** Does `default-features = false` build and pass on remote build host? The fallback is the CAN-injecting OSC length guard (§4).
11. **Paste settle timing.** 100 ms of quiet with a 1 s cap before Enter. Confirm on Kiro, both with and without bracketed paste.
    Resolved (probe, 2026-11-10): all three prompts were submitted with the defaults (`started: true` each time, working within the 2 s start wait). The probe did not record whether kiro-cli had bracketed paste on; the without-paste case is covered only by the `cat` tests.
12. **`tcgetpgrp` on a macOS PTY master** for interrupt's "shell got its prompt back". If it does not work, that case reports `unknown`, as before.
    Resolved (measured on remote build host, FEAT-002): `tcgetpgrp` on the master returns the session leader's group (test `tcgetpgrp_on_the_master_names_the_session_group`), so the path is usable on macOS. The probe did not interrupt Kiro.
13. **Interrupt while blocked.** Esc on Kiro's dialog ("esc to close") is untested under any backend. Parity with hc is kept: the key is sent. Should `interrupt_pane` refuse while blocked instead?
14. **Partial input.** "Pasted, Enter not pressed" maps to `ErrorKind::Other` because the trait has no kind for it. Should the trait gain one, so Shepherd can say exactly what happened?
15. **The default backend** switches to native only if the real-Kiro probe passes. If it fails, hc stays the default, and the failure is reported in the PR.
    Resolved (probe, 2026-11-10): the probe passed (turns 0 → 1 → 2 → 3 → 4 → 5, deny_once left no marker, allow_once created it, 3 Kiro prompts), so `DEFAULT_BACKEND` is `native`.

16. **Residuals from the second review (open).**
    - O-1: the input guard decides on the last *ingested* status. A 7501 report the program wrote but the read thread has not yet parsed can let a guarded write through. This is no worse than hc. A drain-and-ingest of the master under the state lock before the check would narrow it.
    - O-3: arrows under DECCKM with kitty flag 1 use the CSI form (`CSI A`), per Q7. The probe did not exercise it (Kiro leaves DECCKM off); other programs may differ.
    - O-5: finished panes keep their full emulator and raw ring, about 520 MB worst case with 32 retained. Compacting a finished pane to its final screen, or a lower `MAX_FINISHED_PANES`, is a follow-up.
    - O-6: vte ends an OSC on any ESC, so an unterminated 7501 report followed by another escape sequence is accepted as a report. Low risk.
    - Fixed in the same round: an interrupt of a program that never reported status writes the raw byte `0x03` whatever keyboard flags are pushed (O-2); the keychain reader starts with default signal dispositions and an empty mask (O-7); Shepherd refuses to start the native backend if the exit-signal handler cannot be installed (SOL2-N1).

## 16. Consequences

- Shepherd no longer needs herdr-core for its own panes. hc remains available as an optional backend (`OPENAB_SHEPHERD_BACKEND=hc`).
- Conditional input and conditional keys become atomic with respect to status changes and other Shepherd calls, so `answer_dialog` loses the residual race listed under "Residual race on Enter" in the README for this backend. No external client can attach to a native pane: there is no attach and no human viewer. That removes the other source of interleaving, and it also means a person cannot watch a native pane live. Limit: there is no viewer.
- Known limits added to the README and MVP-REPORT:
  - Panes do not survive a Shepherd restart.
  - There is no viewer or attach.
  - Dialog parsing is still Kiro-specific (kiro-cli 3.1.0, V3, shell tool).
  - RIS does not reset kitty flags.
  - 1047 (alternate screen without cursor save) is not emulated.
  - Processes that leave their process group escape removal.
  - No removal tool yet (Q3; since added: `remove_pane`).
- New dependencies: `libc` and `vt100` (with vte, unicode-width, itoa, arrayvec and memchr).
