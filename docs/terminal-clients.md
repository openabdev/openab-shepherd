# OpenAB Shepherd: terminal clients

Shepherd's documented consumer is an MCP client such as Grok Bot. A second
consumer class is a human at a terminal: a terminal client that already gives
the operator a shell where an agent works, and also wants to supervise the
agent itself. The motivating example is OpenAB Connect, the client for
[openab-pty](https://github.com/openabdev/openab-pty). This
page describes the combined shape, how to deploy it, where the client keeps
the token, and the result of checking Shepherd's nine tools against what a
terminal client needs. For the operator reference, see
[operations.md](operations.md). Tracking issue:
[#39](https://github.com/openabdev/openab-shepherd/issues/39); the client
contract is proposed in openab-pty
[#97](https://github.com/openabdev/openab-pty/issues/97).

> **Status.** This page describes a proposed integration. Shepherd and
> openab-pty are separate servers and neither knows about the other.
> Shepherd does not supervise openab-pty sessions and openab-pty is not a
> Shepherd backend; Shepherd contains no openab-pty-specific code and has
> not been run together with an openab-pty session. What works today is
> each server on its own. The client contract is proposed in
> [openab-pty#97](https://github.com/openabdev/openab-pty/issues/97),
> which is still open. How the two share a workspace (same container or a
> shared volume) is the operator's choice and is not tested here.

## The two planes

The terminal client holds at least two independent connections, to two servers:

- **Attach plane (openab-pty).** A shell session over WebSocket: the human's
  own terminal in the pod, a raw byte stream. This is how the operator
  inspects the workspace and works by hand.
- **Supervision plane (Shepherd).** MCP tools over `POST /mcp`: the pane the
  agent CLI runs in — its status, its screen, its permission dialogs.

Neither server proxies the other. Shepherd is not a PTY transport, and the
pty runtime does not proxy or terminate Shepherd's MCP.
The two planes meet only at the workspace: the agent works there under
Shepherd, and the operator's shell opens there under openab-pty.

## Deployment shape

- Shepherd runs where the agent CLI runs: on the operator's host, or in the
  pod beside the CLI (the pod pattern in
  [credentials.md](credentials.md), with the CLI's credential provided by the
  operator through a Secret).
- The supervised agent is started by `start_pane` from a launch profile
  whose allowed directories include the workspace the pty session exposes,
  with the pane's working directory in that workspace. Shepherd, not the
  terminal client and not the pty session, starts the agent.
- The mapping is: one pane ↔ one agent run started by Shepherd; one pty
  session ↔ the operator's shell. They are different terminals. The client
  can show both side by side, but typing in the shell is not typing in the
  pane, and the pane's program is not a process of the shell.
- Keep Shepherd's token file, its profiles file and the CLI's credential
  out of reach of the operator's shell: run Shepherd as a different user
  or in a separate container, and share only the workspace. A shell that
  can read the token file can control every pane and read each profile's
  credential environment.
- Reaching Shepherd: the listener binds a loopback address only. A terminal
  client on the same machine uses the loopback URL directly. A client on
  another machine reaches it over the tailnet through `tailscale serve`,
  with the served name listed in `OPENAB_SHEPHERD_ALLOWED_HOSTS` (see
  [operations.md](operations.md#behind-tailscale-serve)); keep Funnel off.
  The bearer token is required on every request either way.
- Panes are children of Shepherd and end when it restarts. A client that
  reconnects must re-discover panes with `list_panes` and must not cache a
  pane name across server restarts.

## The token, and where a client keeps it

One bearer token controls every pane. Anyone holding it can start programs
(subject to the launch profiles), type into panes, answer permission
dialogs, and remove panes. Shepherd has no per-client authorization (see
[known limits](operations.md#known-limits-mvp-scope)), so for a terminal
client the token is a standing capability over the operator's agents, not a
per-session pass.

A terminal client should therefore store the token in the OS keychain —
the same handling the openab-pty client contract gives its admin
credential — not in a config file, not in the workspace, and not in an
image. The token is separate from both of the client's other credentials:
the pty admin credential and per-session attach tokens, which Shepherd
never sees; and the coding CLI's own credential, which the operator
provides and Shepherd does not read, store, or log
([credentials.md](credentials.md)).

## Checking the tool surface for a terminal client

Each need a terminal client has, checked against the tools as they are
(documented in [operations.md](operations.md#tools)):

| Need | Tool | Result |
|------|------|--------|
| Discover panes and what may be started | `list_panes` | Sufficient. Returns every pane in the pane-object shape, the launch profiles with their allowed directories, `allow_arbitrary_command`, and the live pane count and limit. |
| Start the supervised agent | `start_pane` with `profile` | Sufficient. The profile fixes the program, its arguments and its environment; caller-chosen commands work only when the operator allows arbitrary commands. On a Devin profile, starting in a directory the operator has not trusted fails with `trust_prompt`; the client should surface that to the user. |
| Show a status line | `get_status` | Sufficient. `status` (`{state, kind}`), `reported`, `live`, `turn` and `status_ms` cover it. A program that never reports a status has `reported: false` and counts as working until it exits. |
| Wait for a turn to end or a dialog to appear | `wait_status` (1–1800 s) | Sufficient as a long poll. Branch on `outcome` (`done`, `blocked`, `cancelled`, `exited`, `timeout`, `other`); `turn_complete` is true only for `done` and `exited`. OpenCode reports no `done` state (Shepherd derives `done` from an `idle` after a turn), and `cancelled` means the pane went back to idle after a dialog was denied or cancelled, not that a turn completed. |
| Show what the agent shows | `read_output` | Sufficient as text: the screen, or the last N lines of history, with escape sequences removed. See the limits below: the text is all a client gets. |
| Type a prompt (nudge the agent) | `send_input` | Sufficient while the pane is `idle` or `done`. While the pane is blocked on any dialog, typing is refused with `pane_blocked` and nothing is typed; the client must surface that, not work around it. On a Devin pane, `/yolo` is refused; the client should surface that refusal too. |
| Answer a permission prompt | `answer_dialog` | Sufficient within its scope: `permission` dialogs only, on panes started from a profile that names an `agent`, on the tested shell-command dialogs of the measured Kiro and Devin versions (see known limits), `allow_once` or `deny_once` only. It cannot choose rule-creating options or Devin's Bypass option, so the client must not present an "always allow" control. Each answer should be a human decision on a dialog the human has just seen; a client should not answer automatically or on a timer. |
| Stop a runaway turn | `interrupt_pane` | Sufficient. It sends the backend's stop key and reports the outcome; it does not escalate, so follow it with `wait_status`. |
| End the supervised run | `remove_pane` | Sufficient. It signals the pane's process group and the descendants Shepherd has seen, kills what is left, and frees the slot (at most 8 live panes). |

Conclusion: the nine tools cover the supervision actions a terminal client
needs for permission dialogs on the tested agents. They do not cover
`question` and `auth` dialogs, pane geometry, push events or styled output;
those gaps are recorded below, not fixed here. This page proposes no tool
changes.

## Open gaps and standing limits for terminal clients

- **No way for a human to attach to a pane.** A native pane can be watched
  only through `read_output` and driven only through the tools; there is
  no viewer and no attach (see
  [known limits](operations.md#known-limits-mvp-scope)). The operator's
  pty shell is a different terminal and cannot answer a dialog in the
  pane. A pane blocked on `question` or `auth` can only be handled with
  `interrupt_pane`, `remove_pane`, or by logging the CLI in outside
  Shepherd.
- **Fixed pane geometry.** The native screen is 200 columns × 120 rows with
  2000 lines of scrollback, chosen by Shepherd. No tool sets or reports
  pane dimensions — the pane object has no size field — so a client cannot
  match its window to the pane or reflow the agent's TUI. `read_output`
  trims trailing blank rows, so the text view is usable, but the rendering
  geometry is Shepherd's, not the client's.
- **No push channel.** Only `POST /mcp` is served (`GET` gets 405); there
  is no event stream. A client learns that a pane blocked or finished by
  holding a `wait_status` call open or by polling `get_status` /
  `list_panes`.
- **Text-only screen.** `read_output` strips all escape sequences, and the
  styled screen cells that `answer_dialog` uses internally are never
  returned by a tool. A client can show what the agent's screen says; it
  cannot reproduce its colours or cursor position.
- **Dialogs no tool can answer.** `answer_dialog` covers `kind:
  permission` only. A pane blocked on `kind: auth` needs the operator to
  log the CLI in. A pane blocked on `kind: question` has no tool path at
  all: the in-pane options are `interrupt_pane` or `remove_pane`. This is a
  standing limit for every client, but a human at a terminal is the user
  most likely to expect to type the answer.
- **OpenCode is not verified end to end.** OpenCode v2.2.7 behaviour was
  measured in live sessions on a laptop; Shepherd itself has not been
  checked end to end against OpenCode, and whether the startup `idle`
  report becomes a `get_status` result is still an open point (see the
  README's OpenCode section). A terminal client supervising an OpenCode
  pane inherits those caveats.
