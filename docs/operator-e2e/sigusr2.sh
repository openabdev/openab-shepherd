#!/usr/bin/env bash
# SIGUSR2 on the holder, five times in a row: the front is replaced each time, the holder
# and the pane's program are the same processes, the turn is kept, no backoff is logged,
# and no front is left defunct. Prints how long MCP was down each time.
# Run it yourself (it starts the real server on a throwaway port). Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_begin
holder="$(r_holder_pid)"
r_start t1 turns
r_check "the pane reaches done with 2 turns" r_wait_field t1 turn 2 20
pane_before="$(r_pane_pid)"

worst=0
for i in 1 2 3 4 5; do
  front="$(r_front_pid)"
  t0="$(r_now_ms)"
  kill -USR2 "$holder"
  if r_wait_new_front "$front" 30; then
    ms=$(( $(r_now_ms) - t0 ))
    [ "$ms" -gt "$worst" ] && worst="$ms"
    r_say "round $i: MCP back after $ms ms"
  else
    r_check "round $i: a new front serves MCP" false
  fi
  r_check "round $i: the holder is the same process" test "$(r_holder_pid)" = "$holder"
  r_check "round $i: the pane's program is the same process" test "$(r_pane_pid)" = "$pane_before"
  r_check "round $i: turn is still 2" test "$(r_field t1 turn)" = 2
done
r_check "the slowest restart took under 5 s (was $worst ms)" test "$worst" -lt 5000
r_check "the holder logged five SIGUSR2 restarts" r_log_has 'SIGUSR2: restarting the front, panes untouched' 5
r_check "no backoff was logged" test "$(grep -c 'respawning the front in' "$R_LOG")" = 0
r_check "no front is defunct" test -z "$(r_defunct_children "$holder")"
r_finish
