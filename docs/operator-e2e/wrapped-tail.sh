#!/usr/bin/env bash
# A wrapped tail after a front restart:
#   wrapped-idle    prints more than 1 MiB, then stays silent: it stays `replayed`, and
#                   send_input is refused with pane_replayed however long you wait.
#   wrapped-ticker  prints more than 1 MiB, then a line every 2 s: it leaves `replayed` by
#                   itself, after the first output that follows the adoption nudge.
# (The release of a replayed pane by a human keystroke needs attach, which is a later issue.)
# Run it yourself (it starts the real server on a throwaway port). Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_begin
r_start idle wrapped-idle
r_start ticker wrapped-ticker
r_check "idle: finished its output (done)" r_wait_field idle status.state "done" 60
r_check "ticker: finished its filler (done)" r_wait_field ticker status.state "done" 60
sleep 2

front="$(r_front_pid)"
kill -9 "$front"
r_check "the holder starts another front" r_wait_new_front "$front" 30
r_check "both panes were adopted as replayed" r_log_has 'front: attached; adopted 2 pane\(s\), 2 replayed'

sleep 5
err="$(r_error send_input idle "must not arrive")"
r_check "idle: send_input is refused with pane_replayed after 5 s (got '${err:-none}')" test "$err" = pane_replayed
sleep 10
err="$(r_error send_input idle "must not arrive")"
r_check "idle: still refused after 15 s (got '${err:-none}')" test "$err" = pane_replayed

err="$(r_error send_input ticker "after the repaint")"
r_check "ticker: send_input is no longer refused as replayed (got '${err:-none}')" test "$err" != pane_replayed
r_finish
