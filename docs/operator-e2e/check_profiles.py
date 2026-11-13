#!/usr/bin/env python3
"""Check a Shepherd launch profiles file against the rules in src/profiles.rs
(parse, parse_profile, check_file, LaunchPolicy::load). A re-implementation
for an early warning; the server's own check is the authority. Reads the file
only. Usage: check_profiles.py FILE [--expect-refused]"""
import json, os, re, stat, sys

FILE_KEYS = {"allow_arbitrary_command", "profiles"}
PROFILE_KEYS = {"name", "agent", "permission_mode", "allow_bypass", "argv",
                "extra_args", "cwds", "env"}
AGENTS = {"kiro", "devin", "opencode"}
MODES = {"normal", "accept-edits", "bypass"}
errors = []


def bad(msg):
    errors.append(msg)


def check(path):
    st = os.stat(path)
    if not stat.S_ISREG(st.st_mode):
        bad("not a regular file")
    if st.st_uid != os.geteuid():
        bad("not owned by the current user")
    if st.st_mode & 0o022:
        bad("writable by group or others (chmod 600 or 644)")
    if st.st_size > 1024 * 1024:
        bad("larger than 1 MiB")
    doc = json.load(open(path))
    if not isinstance(doc, dict):
        return bad("top level must be an object")
    for k in doc:
        if k not in FILE_KEYS:
            bad(f"unknown field {k!r}")
    if not isinstance(doc.get("allow_arbitrary_command", False), bool):
        bad("allow_arbitrary_command must be a boolean")
    profiles = doc.get("profiles", [])
    if not isinstance(profiles, list) or len(profiles) > 64:
        return bad("profiles must be a list of at most 64")
    seen = set()
    for i, p in enumerate(profiles):
        if not isinstance(p, dict):
            bad(f"profile {i} must be an object")
            continue
        name = p.get("name")
        if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9_.-]{1,32}", name):
            bad(f"profile {i}: name must be 1-32 of a-z 0-9 - _ .")
            continue
        if name in seen:
            bad(f"two profiles are named {name}")
        seen.add(name)
        at = f"profile {name}"
        for k in p:
            if k not in PROFILE_KEYS:
                bad(f"{at}: unknown field {k!r}")
        argv = p.get("argv")
        if not (isinstance(argv, list) and argv and all(isinstance(a, str) for a in argv)):
            bad(f"{at}: argv must be a non-empty list of strings")
            continue
        if "/" in argv[0] and not argv[0].startswith("/"):
            bad(f"{at}: argv[0] must be a program name or an absolute path")
        if argv[0].startswith("/") and not os.access(argv[0], os.X_OK):
            bad(f"{at}: {argv[0]} is not executable (start_pane would fail with not_found)")
        extra = p.get("extra_args", [])
        allargs = argv + (extra if isinstance(extra, list) else [])
        agent = p.get("agent", "kiro")
        mode = p.get("permission_mode", "normal")
        if agent not in AGENTS:
            bad(f"{at}: agent must be kiro, devin or opencode")
        if mode not in MODES:
            bad(f"{at}: permission_mode must be normal, accept-edits or bypass")
        runs_oc = any("opencode" in a.lower() for a in allargs)
        if agent == "opencode" or runs_oc:
            for a in allargs:
                la = a.lower()
                if "--auto" in la or "autoaccept" in la or "auto-accept" in la:
                    bad(f"{at}: argv/extra_args may not pass {a} for opencode (auto-approve refused)")
        if agent == "opencode" and mode != "normal":
            bad(f"{at}: permission_mode must be normal for opencode")
        if agent == "devin":
            if any(a == "--permission-mode" or a.startswith("--permission-mode=") for a in allargs):
                bad(f"{at}: --permission-mode may not be in argv/extra_args for devin")
            if any(a.startswith("--respect-workspace-trust") for a in allargs):
                bad(f"{at}: --respect-workspace-trust is refused for devin")
        cwds = p.get("cwds")
        if not (isinstance(cwds, list) and 1 <= len(cwds) <= 32):
            bad(f"{at}: cwds must list 1 to 32 directories")
        else:
            for d in cwds:
                if not (isinstance(d, str) and os.path.isabs(d)):
                    bad(f"{at}: cwds: {d!r} is not an absolute path")
                elif not os.path.isdir(d):
                    bad(f"{at}: cwds: {d} is not an existing directory")
        env = p.get("env", {})
        if not isinstance(env, dict) or len(env) > 32:
            bad(f"{at}: env must be an object of at most 32 variables")
        else:
            for k in env:
                if not re.fullmatch(r"[A-Z_][A-Z0-9_]*", k) or k in ("TERM", "PATH") \
                        or k.startswith(("DYLD_", "LD_", "OPENAB_SHEPHERD_", "HC_")):
                    bad(f"{at}: env may not set {k}")


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    expect_refused = "--expect-refused" in sys.argv
    if len(args) != 1:
        sys.exit("usage: check_profiles.py FILE [--expect-refused]")
    check(args[0])
    if expect_refused:
        print("refused as expected:" if errors else "UNEXPECTED: file would load")
        for e in errors:
            print("  -", e)
        sys.exit(0 if errors else 1)
    if errors:
        print("profiles file would be REFUSED:")
        for e in errors:
            print("  -", e)
        sys.exit(1)
    print("profiles file passes the documented rules:", args[0])
