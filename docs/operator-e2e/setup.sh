#!/usr/bin/env bash
# Operator E2E setup for OpenAB Shepherd. Creates the token, the throwaway
# working directories, the OpenCode ask-rules config and two profiles files.
# Starts nothing, calls no model, needs no network.
set -euo pipefail
umask 077

E2E_HOME="${E2E_HOME:-$HOME/.config/shepherd-e2e}"
WORK="${E2E_WORK:-/tmp/shepherd-e2e}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "setup: $*" >&2; exit 1; }

# The token and profiles must live outside every git repository (and outside
# the folder the server starts from, which is $WORK/server below).
if git -C "$HOME" rev-parse --show-toplevel >/dev/null 2>&1; then
  die "$HOME is inside a git repository; set E2E_HOME to a directory that is not"
fi
mkdir -p "$E2E_HOME"
chmod 700 "$E2E_HOME"
if git -C "$E2E_HOME" rev-parse --show-toplevel >/dev/null 2>&1; then
  die "$E2E_HOME is inside a git repository; set E2E_HOME elsewhere"
fi

# The programs, as absolute paths (a profile's argv[0] is a name or an absolute path).
DEVIN_BIN="$(command -v devin || true)"
OPENCODE_BIN="$(command -v opencode || true)"
case "$DEVIN_BIN" in /*) ;; *) die "devin not found as an absolute path on PATH (got: '$DEVIN_BIN')" ;; esac
case "$OPENCODE_BIN" in /*) ;; *) die "opencode not found as an absolute path on PATH (got: '$OPENCODE_BIN')" ;; esac

# Token: refuse to overwrite an existing one.
[ ! -e "$E2E_HOME/token" ] || die "$E2E_HOME/token exists; run cleanup.sh first"
openssl rand -hex 32 > "$E2E_HOME/token"
chmod 600 "$E2E_HOME/token"

# Throwaway directories. No credentials or keys are ever placed in them.
mkdir -p "$WORK/devin" "$WORK/opencode" "$WORK/server"
chmod 700 "$WORK" "$WORK/devin" "$WORK/opencode" "$WORK/server"

# OpenCode: ask rules for shell and edit (the JSON used in the live measurement,
# without the "model" key so your own default model is used). Without an ask
# rule OpenCode runs shell commands with no dialog and Shepherd has nothing to answer.
cat > "$WORK/opencode/opencode.jsonc" <<'JSON'
{
  "$schema": "https://opencode.ai/config.json",
  "permissions": [
    { "action": "shell", "resource": "*", "effect": "ask" },
    { "action": "edit", "resource": "*", "effect": "ask" }
  ]
}
JSON

# Profiles. JSON is built with python so paths are never spliced into text.
DEVIN_BIN="$DEVIN_BIN" OPENCODE_BIN="$OPENCODE_BIN" WORK="$WORK" E2E_HOME="$E2E_HOME" python3 - <<'PY'
import json, os
work, home = os.environ["WORK"], os.environ["E2E_HOME"]
devin = {"name": "devin", "agent": "devin", "permission_mode": "normal",
         "argv": [os.environ["DEVIN_BIN"]], "cwds": [work + "/devin"]}
opencode = {"name": "opencode", "agent": "opencode", "permission_mode": "normal",
            "argv": [os.environ["OPENCODE_BIN"], "--standalone"], "cwds": [work + "/opencode"]}
good = {"allow_arbitrary_command": False, "profiles": [devin, opencode]}
# Deliberately bad: the server must refuse to load it (exit status 2).
bad_oc = dict(opencode, argv=opencode["argv"] + ["--auto"])
bad = {"allow_arbitrary_command": False, "profiles": [bad_oc]}
for name, doc in (("profiles.json", good), ("bad-profiles.json", bad)):
    with open(os.path.join(home, name), "w") as f:
        json.dump(doc, f, indent=2)
        f.write("\n")
PY
chmod 600 "$E2E_HOME/profiles.json" "$E2E_HOME/bad-profiles.json"

python3 -m json.tool "$WORK/opencode/opencode.jsonc" >/dev/null
python3 "$HERE/check_profiles.py" "$E2E_HOME/profiles.json"
echo "token mode:    $(stat -f '%Lp' "$E2E_HOME/token")   (expect 600)"
echo "profiles mode: $(stat -f '%Lp' "$E2E_HOME/profiles.json")   (600 or 644 is accepted)"
echo "setup done. token: $E2E_HOME/token  profiles: $E2E_HOME/profiles.json  workdirs: $WORK/{devin,opencode}"
