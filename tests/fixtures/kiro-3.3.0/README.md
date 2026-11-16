# kiro-cli 3.3.0 permission dialog fixtures

Screens of kiro-cli 3.3.0 (V3 engine) permission dialogs, decoded to plain text
(pyte, 130x45; `shell_wrapped80-1` is 80x45), measured with an agent that trusts no
tools. Each `*.screen.txt` is the screen as a user sees it. They hold no account,
email, quota or credential text.

| Name | Tool line | Expected |
|------|-----------|----------|
| `fs_write-1` | `fs_write → note.txt`, cursor on `Allow once` | accept, cursor 0 |
| `fs_write-2` | same, after one Down | accept, cursor 1 |
| `mcp-1` | `mcp → probe_mcp/probe_ping` | accept, cursor 0 |
| `web_fetch-1` | `web_fetch → example.com` | accept, cursor 0 |
| `web_search-1` | `web_search` (no arrow) | refuse |
| `shell_wrapped80-1` | `shell → …` wrapped over three continuation lines | refuse, "wrapped command" |

## Trust-all startup warning (SYNTHETIC)

`trust-all-warning-synthetic.screen.txt` is **synthetic**: it was written from the
description of the warning in the `-a` investigation, not recorded from kiro-cli. It
carries the phrase `trust all tools mode` and the three options (`No, exit` the
default, `Yes, I accept`, `Yes, and don't ask again`). Replace it with a screen
recorded by `docs/operator-e2e/record-replay-fixture.sh` from a real `-a` start, and
keep the test that includes it. Whether `Yes, and don't ask again` persists is
unmeasured.
