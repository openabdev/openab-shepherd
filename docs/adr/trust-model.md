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
- Restart survival, so a Shepherd restart does not end every pane. This is planned (#66) and does not exist yet.

A pane's agent holds no Shepherd credential unless the operator gives it a client of its
own. The `routine` scope lets such a client make Shepherd type a fixed, bounded line into
panes whose profile opted in with `dynamic_routines`; it does not imply `control`, and the
agent, running as the same account, could read any token file that account can read, so
the grant is a narrow separate token, not the operator's.

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
