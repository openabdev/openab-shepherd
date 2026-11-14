#!/usr/bin/env bash
# Advisory stress run for the native PTY tests (issue #55).
#
# Usage: stress-native.sh <loops> <threads: 1|default|both>
#
# Starts one `yes > /dev/null` per core, then runs
# `cargo test --locked --lib native::tests::` <loops> times for each thread
# setting. Failing test names are parsed from each run's output. A run that
# exits non-zero without naming a failed test (build error, panic outside a
# test, signal) is counted under a placeholder name. The table and totals go
# to stdout and to $GITHUB_STEP_SUMMARY. Exit status is 1 if any run failed.
# The workers are left running; the workflow kills them in an `if: always()` step.
set -uo pipefail

loops=${1:-20}
threads=${2:-both}
filter='native::tests::'
NO_NAME='<no test name parsed>'

case "$loops" in '' | *[!0-9]* | 0) echo "loops must be a positive integer: '$loops'" >&2; exit 2 ;; esac
case "$threads" in 1 | default | both) ;; *) echo "threads must be 1, default or both: '$threads'" >&2; exit 2 ;; esac

case "$(uname -s)" in
  Darwin) cores=$(sysctl -n hw.ncpu) ;;
  *) cores=$(nproc) ;;
esac

work=$(mktemp -d)
settings=()
[ "$threads" != default ] && settings+=(1)
[ "$threads" != 1 ] && settings+=(default)

# One record per failed run: "<setting>\t<test name or placeholder>".
: > "$work/failures.tsv"
: > "$work/runs.tsv" # "<setting>\t<run index>\t<exit code>"

report() {
  {
    echo "### Native tests under CPU load"
    echo
    echo "Runner: $(uname -s) $(uname -m), $cores busy workers, $loops loops per setting."
    echo
    echo "| threads | runs | failed runs |"
    echo "|---|---|---|"
    for s in "${settings[@]}"; do
      runs=$(awk -F'\t' -v s="$s" '$1 == s' "$work/runs.tsv" | wc -l | tr -d ' ')
      bad=$(awk -F'\t' -v s="$s" '$1 == s && $3 != 0' "$work/runs.tsv" | wc -l | tr -d ' ')
      echo "| $s | $runs | $bad |"
    done
    echo
    if [ -s "$work/failures.tsv" ]; then
      echo "| test | threads | failed runs |"
      echo "|---|---|---|"
      sort "$work/failures.tsv" | uniq -c | awk '{
        n = $1; sub(/^ *[0-9]+ /, ""); split($0, f, "\t"); print "| `" f[2] "` | " f[1] " | " n " |" }'
    else
      echo "No failures."
    fi
  } > "$work/summary.md"
  cat "$work/summary.md"
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] && cat "$work/summary.md" >> "$GITHUB_STEP_SUMMARY"
  return 0
}
trap report EXIT

echo "starting $cores busy workers"
for _ in $(seq "$cores"); do
  yes > /dev/null &
done
sleep 2
if ! pgrep -x yes > /dev/null; then
  echo "busy workers are not running" >&2
  exit 2
fi

failed=0
for s in "${settings[@]}"; do
  args=(--locked --lib "$filter")
  [ "$s" = 1 ] && args+=(-- --test-threads=1)
  for i in $(seq "$loops"); do
    log="$work/run-$s-$i.log"
    cargo test "${args[@]}" > "$log" 2>&1
    rc=$?
    printf '%s\t%s\t%s\n' "$s" "$i" "$rc" >> "$work/runs.tsv"
    if [ "$rc" -ne 0 ]; then
      failed=1
      names=$(sed -n 's/^test \(.*\) \.\.\. FAILED$/\1/p' "$log" | sort -u)
      if [ -z "$names" ]; then
        names="$NO_NAME (exit $rc)"
        tail -n 30 "$log" | sed 's/^/    /'
      fi
      echo "run threads=$s #$i: exit $rc"
      printf '%s\n' "$names" | while IFS= read -r n; do
        printf '%s\t%s\n' "$s" "$n" >> "$work/failures.tsv"
        echo "  FAILED $n"
      done
    else
      echo "run threads=$s #$i: ok"
    fi
  done
done

exit "$failed"
