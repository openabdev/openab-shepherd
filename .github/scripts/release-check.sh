#!/usr/bin/env bash
# Usage: release-check.sh <version>   (version without the leading v, e.g. 0.3.0)
# Fails unless Cargo.toml's package version equals <version> and CHANGELOG.md has a
# "## <version>" section. Run from the repository root.
set -euo pipefail

version="${1:-}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version '$version' is not X.Y.Z" >&2
  exit 1
fi

cargo_version=$(awk '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/^[^"]*"|"[^"]*$/, ""); print; exit }
' Cargo.toml)
if [ "$cargo_version" != "$version" ]; then
  echo "error: Cargo.toml version '${cargo_version:-<none>}' != '$version'" >&2
  exit 1
fi

if ! grep -qxF "## $version" CHANGELOG.md; then
  echo "error: CHANGELOG.md has no '## $version' section" >&2
  exit 1
fi
echo "ok: Cargo.toml and CHANGELOG.md match $version"
