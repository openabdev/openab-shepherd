#!/usr/bin/env bash
# Usage: release-notes.sh <version> <output-file>
# Writes the body of the "## <version>" section of CHANGELOG.md (up to the next
# "## " heading, without the heading itself) to <output-file>. Fails if the
# section is missing or has no text. Run from the repository root.
set -euo pipefail

version="${1:-}"
out="${2:-}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || [ -z "$out" ]; then
  echo "usage: release-notes.sh X.Y.Z <output-file>" >&2
  exit 1
fi

awk -v want="## $version" '
  $0 == want { found = 1; next }
  found && /^## / { exit }
  found { print }
' CHANGELOG.md > "$out"

# Trim leading and trailing blank lines.
awk 'NF { started = 1 } started { buf[++n] = $0 } END { while (n > 0 && buf[n] ~ /^[[:space:]]*$/) n--; for (i = 1; i <= n; i++) print buf[i] }' "$out" > "$out.tmp"
mv "$out.tmp" "$out"

if ! grep -q '[^[:space:]]' "$out"; then
  echo "error: CHANGELOG.md section '## $version' is missing or empty" >&2
  exit 1
fi
echo "ok: wrote $(wc -l < "$out") lines of notes for $version to $out"
