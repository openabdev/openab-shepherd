#!/usr/bin/env bash
# Run the test executables listed in a file, one after the other, and keep the
# run observable (issue #108: a macOS job hung to the cancel with no log).
#
# Usage: ci/run-tests.sh <executables-file> [--doc]
#
#   <executables-file>  one test executable per line (see the Build step of ci.yml)
#   --doc               after the executables, run `cargo test --locked --doc`
#
# For each executable the output is teed to $CI_LOG_DIR/<name>.log. Every
# $HEARTBEAT_SECS (60) a line shows elapsed time, uptime, free memory, the count
# and text of the last finished test and the names of the processes under the
# test. A frozen VM stops the heartbeat; a hung test keeps it going with a stale
# "last finished". If no test finishes for $HANG_SECS (300) the script dumps
# `ps`, `lsof` and (macOS) `sample` into $CI_LOG_DIR/diagnostics.txt and to the
# log, kills the test's process group with SIGKILL and exits 1 with a line
# `HANG: no progress for <n> s in <binary>; last finished: <line>`.
# The same dump runs when the step is cancelled (SIGINT/SIGTERM).
#
# It never retries a test and never changes the test thread count. Every wait is
# bounded: the polling loop ends when the process exits or the hang rule fires,
# and each diagnostic command runs under a kill timer.
set -uo pipefail
set -m # one process group per background job, so a hang can be killed whole

list=${1:-}
doc=no
[ "${2:-}" = "--doc" ] && doc=yes
if [ -z "$list" ] || [ ! -f "$list" ]; then
  echo "usage: $0 <executables-file> [--doc]" >&2
  exit 2
fi

HEARTBEAT_SECS=${HEARTBEAT_SECS:-60}
HANG_SECS=${HANG_SECS:-300}
POLL_SECS=${POLL_SECS:-1}
CI_LOG_DIR=${CI_LOG_DIR:-ci-logs}
mkdir -p "$CI_LOG_DIR"
DIAG="$CI_LOG_DIR/diagnostics.txt"
: > "$DIAG"

os=$(uname -s)
cur_pid=""
cur_name=""
cur_log=""
tail_pid=""

# bounded <secs> <cmd...>: run a command, SIGKILL it after <secs>. Output of the
# command is kept; the timer holds no stdout, so it cannot keep a pipe open.
bounded() {
  local secs=$1 p w rc
  shift
  "$@" &
  p=$!
  (
    exec > /dev/null 2>&1 < /dev/null
    sleep "$secs"
    kill -KILL "$p"
  ) &
  w=$!
  wait "$p" 2> /dev/null
  rc=$?
  pkill -P "$w" 2> /dev/null
  kill "$w" 2> /dev/null
  wait "$w" 2> /dev/null
  return $rc
}

# Pids of the test process, its descendants and everything still in its group.
tree_pids() {
  ps -axo pid=,ppid=,pgid= 2> /dev/null | awk -v root="$1" '
    { n++; pid[n] = $1; par[$1] = $2; grp[$1] = $3 }
    END {
      mark[root] = 1
      for (i = 1; i <= n; i++) if (grp[pid[i]] == root) mark[pid[i]] = 1
      do {
        ch = 0
        for (i = 1; i <= n; i++)
          if (!(pid[i] in mark) && (par[pid[i]] in mark)) { mark[pid[i]] = 1; ch = 1 }
      } while (ch)
      for (k in mark) print k
    }' | sort -n
}

csv() { tr '\n' ',' | sed 's/,$//'; }

# Names of the processes under the test, e.g. "sleep x2 shepherd-1a2b x1".
proc_names() {
  local ids
  ids=$(tree_pids "$1" | csv)
  [ -n "$ids" ] || { echo "none"; return; }
  ps -p "$ids" -o comm= 2> /dev/null | awk -F/ '{ c[$NF]++ } END { for (k in c) printf "%s x%d ", k, c[k] }'
}

free_mem() {
  if [ "$os" = Darwin ]; then
    local ps
    ps=$(sysctl -n hw.pagesize 2> /dev/null || echo 4096)
    vm_stat 2> /dev/null | awk -v ps="$ps" '
      /Pages free/ { f = $3 + 0 }
      /Pages inactive/ { i = $3 + 0 }
      /Pages speculative/ { s = $3 + 0 }
      END { printf "%d MiB free+inactive", (f + i + s) * ps / 1048576 }'
  else
    free -m 2> /dev/null | awk '/^Mem:/ { printf "%d MiB available of %d", $7, $2 }'
  fi
}

load() { uptime | sed 's/^.*up /up /'; }

finished_count() { grep -cE '^test .* \.\.\. ' "$1" 2> /dev/null || true; }
last_finished() { grep -E '^test .* \.\.\. ' "$1" 2> /dev/null | tail -n 1; }

# Tests listed by the executable but not yet finished, in listing order. Test
# threads take tests in that order, so the ones that are running are the first
# few of these.
unfinished_tests() {
  local exe=$1 log=$2 all=$3
  [ -s "$all" ] || return 0
  sed -n 's/^test \(.*\) \.\.\. .*/\1/p' "$log" | sort > "$all.done"
  grep -vxFf "$all.done" "$all" | head -n 10
}

dump_diagnostics() {
  local pid=$1 name=$2 log=$3 all=$4 why=$5 ids
  {
    echo "===== diagnostics for $name: $why ($(date -u +%FT%TZ)) ====="
    echo "--- uptime / memory"
    load
    free_mem
    echo
    echo "--- tests listed but not finished (the running ones are among the first)"
    unfinished_tests "" "$log" "$all"
    echo "--- last finished: $(last_finished "$log")"
    echo "--- ps (all processes)"
    ps -axo pid,ppid,pgid,stat,etime,command 2>&1 | head -n 400
    ids=$(tree_pids "$pid" | csv)
    echo "--- processes under the test: ${ids:-none}"
    if [ -n "$ids" ]; then
      echo "--- lsof -nP"
      bounded 30 lsof -nP -p "$ids" 2>&1 | head -n 300
      if [ "$os" = Darwin ]; then
        for p in $(tree_pids "$pid" | head -n 6); do
          echo "--- sample $p 3"
          bounded 30 sample "$p" 3 2>&1 | head -n 70
        done
      else
        echo "--- threads (state and kernel wait channel)"
        ps -L -p "$ids" -o pid,tid,stat,wchan:24,comm 2>&1 | head -n 200
      fi
    fi
    echo "===== end of diagnostics ====="
  } 2>&1 | tee -a "$DIAG"
}

kill_group() {
  local pid=$1
  kill -KILL -- "-$pid" 2> /dev/null
  kill -KILL "$pid" 2> /dev/null
  for p in $(tree_pids "$pid"); do kill -KILL "$p" 2> /dev/null; done
  wait "$pid" 2> /dev/null
}

stop_tail() {
  if [ -n "$tail_pid" ]; then
    sleep 1 # let the follower print the last lines
    kill "$tail_pid" 2> /dev/null
    wait "$tail_pid" 2> /dev/null
    tail_pid=""
  fi
}

on_cancel() {
  echo "CANCELLED: the step was stopped while running ${cur_name:-nothing}"
  if [ -n "$cur_pid" ]; then
    dump_diagnostics "$cur_pid" "$cur_name" "$cur_log" "$cur_log.all" "step cancelled or timed out"
    kill_group "$cur_pid"
  fi
  stop_tail
  exit 130
}
trap on_cancel INT TERM HUP

failed=()
ran=0

# run_one <name> <log> <cmd...>
run_one() {
  local name=$1 log=$2 start now last_progress last_beat n seen idle
  shift 2
  : > "$log"
  if [ "${LIST_TESTS:-yes}" = yes ] && [ "$name" != doctests ]; then
    bounded 60 "$1" --list --format terse 2> /dev/null | sed -n 's/: test$//p' > "$log.all"
  else
    : > "$log.all"
  fi
  echo "::group::$name"
  echo "[run] $name ($(wc -l < "$log.all" | tr -d ' ') tests listed)"
  tail -n +1 -f "$log" &
  tail_pid=$!
  "$@" > "$log" 2>&1 < /dev/null &
  cur_pid=$!
  cur_name=$name
  cur_log=$log
  start=$(date +%s)
  last_progress=$start
  last_beat=$start
  seen=0
  while kill -0 "$cur_pid" 2> /dev/null; do
    sleep "$POLL_SECS"
    now=$(date +%s)
    n=$(finished_count "$log")
    if [ "$n" != "$seen" ]; then
      seen=$n
      last_progress=$now
    fi
    if [ $((now - last_beat)) -ge "$HEARTBEAT_SECS" ]; then
      last_beat=$now
      echo "[heartbeat] $name elapsed=$((now - start))s idle=$((now - last_progress))s finished=$n | $(load) | mem: $(free_mem) | procs: $(proc_names "$cur_pid")| last: $(last_finished "$log")" | tee -a "$CI_LOG_DIR/heartbeat.log"
    fi
    idle=$((now - last_progress))
    if [ "$idle" -ge "$HANG_SECS" ] && kill -0 "$cur_pid" 2> /dev/null; then
      stop_tail
      echo "::endgroup::"
      echo "HANG: no progress for ${idle} s in $name; last finished: $(last_finished "$log")"
      dump_diagnostics "$cur_pid" "$name" "$log" "$log.all" "no progress for ${idle} s"
      kill_group "$cur_pid"
      cur_pid=""
      echo "HANG: no progress for ${idle} s in $name; last finished: $(last_finished "$log")"
      exit 1
    fi
  done
  wait "$cur_pid"
  local rc=$?
  cur_pid=""
  stop_tail
  echo "::endgroup::"
  ran=$((ran + 1))
  if [ "$rc" -ne 0 ]; then
    echo "[fail] $name exited with status $rc"
    failed+=("$name")
  else
    echo "[ok] $name ($(finished_count "$log") tests finished, $(($(date +%s) - start)) s)"
  fi
}

while IFS= read -r exe; do
  [ -n "$exe" ] || continue
  run_one "$(basename "$exe")" "$CI_LOG_DIR/$(basename "$exe").log" "$exe"
done < "$list"

if [ "$doc" = yes ]; then
  LIST_TESTS=no run_one doctests "$CI_LOG_DIR/doctests.log" cargo test --locked --doc
fi

if [ "${#failed[@]}" -gt 0 ]; then
  echo "FAILED: ${failed[*]} (of $ran test runs)"
  exit 1
fi
echo "All $ran test runs passed"
