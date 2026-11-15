#!/usr/bin/env bash
# Kill the front while two panes show a Kiro permission dialog (a recorded one, no model).
#   dialog-live     the dialog sits on a short tail: answer_dialog works after the restart
#                   and the program receives exactly the Enter.
#   dialog-wrapped  more than 1 MiB was printed before the dialog: the tail has wrapped, the
#                   pane is `replayed`, answer_dialog is refused with pane_replayed, and the
#                   program receives nothing.
# Run it yourself (it starts the real server on a throwaway port). Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_begin
r_start live dialog-live
r_start wrapped dialog-wrapped
r_check "live: blocked on a permission dialog" r_wait_field live status.kind permission 20
r_check "wrapped: blocked on a permission dialog" r_wait_field wrapped status.kind permission 60
# Let the front write its snapshot to the holder before it dies.
sleep 2

front="$(r_front_pid)"
kill -9 "$front"
r_check "the holder starts another front" r_wait_new_front "$front" 30
r_check "both panes were adopted, the wrapped one as replayed" r_log_has 'front: attached; adopted 2 pane\(s\), 1 replayed'

live_dir="$R_WORK/dialog-live"
wrapped_dir="$R_WORK/dialog-wrapped"
r_check "live: still blocked after the restart" test "$(r_field live status.kind)" = permission
err="$(r_error answer_dialog live allow_once)"
r_check "live: answer_dialog is accepted (error: '${err:-none}')" test -z "$err"
r_check "live: the program received the Enter" r_wait_file "$live_dir/received.txt" 10

err="$(r_error answer_dialog wrapped allow_once)"
r_check "wrapped: answer_dialog is refused with pane_replayed (got '${err:-none}')" test "$err" = pane_replayed
sleep 2
r_check "wrapped: the program received nothing" test ! -e "$wrapped_dir/received.txt"
r_finish
