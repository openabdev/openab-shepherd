#!/usr/bin/env python3
"""The structural checks of dialog-canary.sh. Starts nothing, calls no model, reads two files.

  dialog-canary-check.py structure AGENT SCREEN_FILE [--marker NAME] [--fixture FILE]
      Reduce the live screen to its skeleton (the title or heading, the tool and its command
      line, the options in order) and compare it with the skeleton of the recorded fixture
      (kiro, opencode) or of the layout the dialect module defines (devin, which has no fixture
      file). Account names, paths, banners and the model's own wording differ from run to run, so
      the comparison is never byte for byte. Exit 0 and `OK ...` when they agree, exit 1 and
      `FAIL ...` (what was expected, what was seen) when not, exit 2 on a usage error.

  dialog-canary-check.py login SCREEN_FILE
      Exit 0 and `LOGIN ...` when the screen asks the user to log in, exit 1 when it does not.

The skeleton rules follow src/dialog.rs (Kiro), src/devin_dialog.rs and src/opencode_dialog.rs.
The Rust test tests/dialog_canary.rs runs this file on the committed fixtures and on broken
copies and requires the real parsers to agree with it on every one.
"""
import re
import sys

KIRO_SUFFIX = "requires approval"
KIRO_ARROW = " \u2192 "
KIRO_CURSOR = "\u276f"
KIRO_OPTIONS = ["Allow once", "Deny once", "Create allow rule", "Create deny rule"]
DEVIN_HEADING = "Running command"
DEVIN_BULLET = "\u23fa"
DEVIN_BRANCH = "\u2514"
DEVIN_CURSOR = "\u276d"
DEVIN_OPTIONS = ["Allow", "Bypass mode", "Deny"]
OC_HEADING = "\u25b3 Permission required"
OC_BAR = "\u2503"
OC_OPTIONS = ["Allow once", "Always allow", "Reject"]
OC_HINT = "ctrl+f fullscreen"

LOGIN_PATTERNS = [
    r"not logged in",
    r"please (log|sign) ?in",
    r"kiro-cli login",
    r"devin auth login",
    r"opencode auth login",
    r"open (this|the following) (url|link)",
    r"browser (to|and) (log|sign)",
    r"log in (to|with)",
    r"sign in (to|with)",
]


class Mismatch(Exception):
    pass


def skeleton(agent, screen):
    """The structure of the one dialog on the screen; Mismatch when there is not exactly one."""
    return {"kiro": _kiro, "devin": _devin, "opencode": _opencode}[agent](screen.splitlines())


def _kiro(lines):
    titles = [i for i, l in enumerate(lines) if l.rstrip().endswith(KIRO_SUFFIX)]
    if len(titles) != 1:
        raise Mismatch(f"{len(titles)} lines end in {KIRO_SUFFIX!r}, expected one")
    i = titles[0]
    command = lines[i + 1].strip() if i + 1 < len(lines) else ""
    if KIRO_ARROW not in command:
        raise Mismatch(f"the line after the title is not '<tool> \u2192 <command>': {command!r}")
    tool, rest = command.split(KIRO_ARROW, 1)
    j = i + 2
    while j < len(lines) and not lines[j].strip():
        j += 1
    block = [l.strip() for l in lines[j:j + 4]]
    if len(block) < 4:
        raise Mismatch(f"expected four option lines, found {len(block)}")
    cursors = sum(1 for l in block if l.startswith(KIRO_CURSOR))
    labels = [l[len(KIRO_CURSOR):].strip() if l.startswith(KIRO_CURSOR) else l for l in block]
    names = []
    for label, want in zip(labels, KIRO_OPTIONS):
        names.append(want if (label == want or (want.startswith("Create") and label.startswith(want))) else label[:60])
    return {"title_suffix": KIRO_SUFFIX, "tool": tool.strip(), "options": names,
            "cursors": cursors, "command": rest.strip()}


def _devin(lines):
    def heading(line):
        line = line.strip()
        if line.startswith(DEVIN_BULLET):
            line = line[len(DEVIN_BULLET):].strip()
        return line == DEVIN_HEADING

    heads = [i for i, l in enumerate(lines) if heading(l)]
    if len(heads) != 1:
        raise Mismatch(f"{len(heads)} lines read {DEVIN_HEADING!r}, expected one")
    j = heads[0] + 1
    while j < len(lines) and not lines[j].strip():
        j += 1
    line = lines[j].strip() if j < len(lines) else ""
    if line.startswith(DEVIN_BRANCH):
        line = line[len(DEVIN_BRANCH):].strip()
    if not line.startswith("$ "):
        raise Mismatch(f"the line after the heading is not '$ <command>': {line!r}")
    j += 1
    while j < len(lines) and not lines[j].strip():
        j += 1
    rows = [l.strip() for l in lines[j:j + 3]]
    if len(rows) < 3:
        raise Mismatch(f"expected three option rows, found {len(rows)}")
    cursors = sum(1 for r in rows if r.startswith(DEVIN_CURSOR))
    labels = [r[len(DEVIN_CURSOR):].strip() if r.startswith(DEVIN_CURSOR) else r for r in rows]
    names = []
    for label, want in zip(labels, DEVIN_OPTIONS):
        ok = label == want or (want == "Allow" and label.startswith("Allow "))
        names.append(want if ok else label[:60])
    return {"heading": DEVIN_HEADING, "tool": "$", "options": names, "cursors": cursors,
            "command": line[2:].strip()}


def _opencode(lines):
    def content(line):
        return line.lstrip(" \t" + OC_BAR).rstrip()

    rows = [content(l) for l in lines]
    heads = [i for i, l in enumerate(rows) if l.startswith(OC_HEADING)]
    if len(heads) != 1:
        raise Mismatch(f"{len(heads)} lines start {OC_HEADING!r}, expected one")
    body = [l for l in rows[heads[0] + 1:] if l]
    if not body or not body[0].startswith("$ "):
        raise Mismatch(f"the line after the heading is not '$ <command>': {body[:1]!r}")
    command = body[0][2:].strip()
    option_rows = [l for l in body[1:] if l.startswith(OC_OPTIONS[0])]
    if len(option_rows) != 1:
        raise Mismatch(f"{len(option_rows)} option rows start {OC_OPTIONS[0]!r}, expected one")
    rest = option_rows[0]
    names = []
    for want in OC_OPTIONS:
        if not rest.startswith(want):
            names.append(rest.split("  ")[0] or "?")
            break
        names.append(want)
        rest = rest[len(want):].lstrip()
    if len(names) == 3 and not rest.startswith(OC_HINT):
        names.append(rest[:20] or "<nothing>")
    return {"heading": OC_HEADING, "tool": "$", "options": names, "command": command}


def comparable(sk):
    """The part of a skeleton that two runs must share: not the command, not the cursor."""
    return {k: v for k, v in sk.items() if k not in ("command", "cursors")}


def expected_for(agent, fixture):
    if fixture:
        with open(fixture, encoding="utf-8") as f:
            return comparable(skeleton(agent, f.read()))
    # Devin has no fixture file: the layout src/devin_dialog.rs measured on 3000.12.57.
    if agent == "devin":
        return {"heading": DEVIN_HEADING, "tool": "$", "options": list(DEVIN_OPTIONS)}
    raise SystemExit(f"dialog-canary-check: agent {agent} needs --fixture")


def structure(agent, screen_file, marker, fixture):
    with open(screen_file, encoding="utf-8", errors="replace") as f:
        screen = f.read()
    try:
        want = expected_for(agent, fixture)
    except Mismatch as err:
        print(f"ERROR the fixture does not parse: {err}")
        return 2
    try:
        got = skeleton(agent, screen)
    except Mismatch as err:
        print(f"FAIL no dialog of the expected layout on the screen: {err}")
        return 1
    problems = []
    for key, value in comparable(got).items():
        if want.get(key) != value:
            problems.append(f"{key}: expected {want.get(key)!r}, seen {value!r}")
    if got.get("cursors", 1) != 1:
        problems.append(f"expected one cursor among the options, seen {got['cursors']}")
    if marker and marker not in got["command"]:
        problems.append(f"the command does not name the marker {marker!r}: {got['command']!r}")
    if problems:
        print("FAIL " + "; ".join(problems))
        return 1
    print(f"OK {agent} dialog: tool {got['tool']!r}, options {got['options']}")
    return 0


def login(screen_file):
    with open(screen_file, encoding="utf-8", errors="replace") as f:
        text = f.read().lower()
    for pattern in LOGIN_PATTERNS:
        if re.search(pattern, text):
            print(f"LOGIN the screen matches {pattern!r}")
            return 0
    return 1


def main(argv):
    if len(argv) >= 3 and argv[0] == "structure" and argv[1] in ("kiro", "devin", "opencode"):
        marker = fixture = None
        rest = argv[3:]
        while rest:
            if rest[0] == "--marker" and len(rest) >= 2:
                marker, rest = rest[1], rest[2:]
            elif rest[0] == "--fixture" and len(rest) >= 2:
                fixture, rest = rest[1], rest[2:]
            else:
                break
        if rest:
            print(__doc__, file=sys.stderr)
            return 2
        return structure(argv[1], argv[2], marker, fixture)
    if len(argv) == 2 and argv[0] == "login":
        return login(argv[1])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
