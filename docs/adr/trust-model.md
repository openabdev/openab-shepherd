# ADR: Trust model: one account, pane programs trusted

- **Status:** Accepted
- **Supersedes:** the mitigation items in [native-core.md](native-core.md) and the planned phases in issue #4. That document is kept as written; where it describes a defense against a same-account process, this ADR wins.

## Premise

Shepherd runs for one user under one OS account. The programs that run in its panes (coding agents and the tools they start) are inside the trust boundary, the same as the user's own shell. There is no over-defense: a mitigation whose only purpose is to stop another process of the same account is not built, and is removed when found.

What Shepherd does guard is the boundary between that account and the clients that call it over MCP, such as remote bots and CI jobs, and the operator's own mistakes.

## What Shepherd adds over herdr and Orca

herdr and Orca also trust the agent in the pane and the account it runs under. Shepherd adds three things for remote and shared use:

- Per-client scopes and an audit log. A status-only CI bot cannot approve a dialog, run an arbitrary command or read the screen, and each mutating call is recorded against its client.
- A guarded `answer_dialog`. It holds the pane lock from the first read to the Enter key and re-checks status, dialog kind and turn, so a person typing in the same pane and a parser misread do not produce a wrong approval.
- Restart survival under `hold`, so a Shepherd restart does not end every pane (#76). Panes still end when the holder stops.

A pane's agent holds no Shepherd credential unless the operator gives it a client of its
own. The `routine` scope lets such a client make Shepherd type a fixed, bounded line into
panes whose profile opted in with `dynamic_routines`; it does not imply `control`, and the
agent, running as the same account, could read any token file that account can read, so
the grant is a narrow separate token, not the operator's.

## Trusted clients

A client is not an attacker just because it is remote. A bot or an orchestrating agent that an operator trusts is inside the boundary for what it was granted, and Shepherd makes that explicit instead of switching the protections off. Three layers stay apart:

1. **Authentication and attribution.** A bearer token per client, and an audit line for every state-changing call. These do not relax: the audit is how a trusted client stays accountable.
2. **Launch shape.** What a client may start, and with which flags. This is the layer a trusted client widens, through grants the operator writes: `start` with a short `profiles` list (the operator's programs), `start:args` (flags appended to those programs, run with their credentials), and `start:arbitrary` with the server's `allow_arbitrary_command` (the caller's own program, never with a credential). Each is one more reason to read a clients file entry carefully, and none is on by default for a client or for the single token.
3. **Correctness and trust-effect protections.** The blocked-pane guard, `answer_dialog`'s allow-once and deny-once choices, the input rules, the protected environment names, the pane cap and the rule that Shepherd never answers a dialog that grants trust (the Kiro trust-all warning, the Devin trust prompt). These keep a pane from receiving a wrong key whoever is calling, so a trusted client does not relax them.

The recipe for a trusted orchestrator, using only existing settings:

```
profiles file:  {"allow_arbitrary_command": true, "profiles": [ ... ]}
clients file:   {"id": "orchestrator",
                 "scopes": ["observe", "read", "control", "start", "start:arbitrary",
                            "start:args", "dialog", "remove"],
                 "profiles": ["kiro-sandbox"]}
```

Leave `allow_arbitrary_command` false and drop `start:arbitrary` if the client should only steer the operator's profiles. There is no posture switch that does this in one setting: the three choices (server flag, scopes, profile list) are the posture, and a macro would hide which of them a client really has. A single-token deployment holds `start` and `start:arbitrary` but not `start:args`, so it cannot pass call-level flags; it needs a clients file (see [operations.md](../operations.md#who-can-use-it-a-single-token-cannot)).

herdr passes any argument from any socket caller and Orca from any owner-socket caller; neither tells clients apart. Shepherd tells them apart over HTTP, so flags and programs are grants, not defaults.

## Decision test

Keep a mitigation if it stops at least one of these:

1. An operator mistake, such as a token committed to the repository or a public bind.
2. Remote-bot overreach, such as a client using a scope it was not given.
3. A real field bug, such as output loss or an interleaved write, backed by a measurement.

Do not build or keep it if it only stops a same-account attacker or a pane program acting against Shepherd.

## Non-goals

Shepherd does not defend against, and no doc should claim a defense against:

- A same-account attacker or a hostile pane program.
- `ptrace`, `/proc` reads and other same-account inspection of the server.
- Socket squatting.
- File-descriptor races beyond setting `FD_CLOEXEC` under the spawn lock.
- Status forgery: a pane can print its own status marker, and Shepherd does not treat that as a security boundary.
- Symlink swaps of files the account already owns.
- Kernel attestation of peers.

The phases once planned in #4 are non-goals as well: per-client pane ownership, an audit hash chain, audit rotation, a token command and a `dialog:deny` setting. Revisit one only when a concrete operator mistake or remote-bot problem calls for it.

## Freeze new screen dialects

The per-agent dialog parsers in place today stay. No new screen-scraped dialect is added without a measured fixture and a version pin. Claude Code and Codex are planned and have documented permission hooks, which will be used instead of screen parsing.
