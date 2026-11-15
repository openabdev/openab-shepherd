#!/usr/bin/env bash
# The routine across a front restart (takes about two minutes):
#   routine-giveup  max_consecutive_nudges is 1: it is nudged once and the routine gives
#                   up. After SIGUSR2 it must stay given up (one line in typed.txt, ever).
#   routine-fresh   has not been nudged when SIGUSR2 arrives; its profile allows two
#                   nudges (max_consecutive_nudges=2). The new front must rebuild its
#                   watch, nudge it at least once and at most twice (the idle clock
#                   restarts), and log that it gave up.
# Then the profile of routine-giveup loses its `routine` block and SIGUSR2 is sent again:
# the front must log that it no longer watches the pane, and the fresh pane, which has
# given up, must not be nudged again by the front after that second restart.
# Run it yourself (it starts the real server on a throwaway port). Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

r_begin
holder="$(r_holder_pid)"
giveup_file="$R_WORK/routine-giveup/typed.txt"
fresh_file="$R_WORK/routine-fresh/typed.txt"

r_start giveup routine-giveup
r_check "giveup: the pane is done" r_wait_field giveup status.state "done" 20
end=$((SECONDS + 60))
while [ "$SECONDS" -lt "$end" ] && [ "$(r_file_lines "$giveup_file")" -lt 1 ]; do sleep 1; done
r_check "giveup: it was nudged once" test "$(r_file_lines "$giveup_file")" = 1
end=$((SECONDS + 40))
while [ "$SECONDS" -lt "$end" ] && ! r_log_has 'routine gave up: pane giveup'; do sleep 1; done
r_check "giveup: the routine gave up" r_log_has 'routine gave up: pane giveup'

# The fresh pane starts only now, so its first nudge is still 10 s or more away.
r_start fresh routine-fresh
r_check "fresh: the pane is done" r_wait_field fresh status.state "done" 20
r_check "fresh: no nudge before the restart" test "$(r_file_lines "$fresh_file")" = 0
front="$(r_front_pid)"
kill -USR2 "$holder"
restart_at=$SECONDS
r_check "a new front serves MCP" r_wait_new_front "$front" 30

end=$((SECONDS + 60))
while [ "$SECONDS" -lt "$end" ] && [ "$(r_file_lines "$fresh_file")" -lt 1 ]; do sleep 1; done
# The routine types the nudge before it logs it, so wait for the log lines instead of
# reading the log once. The second nudge comes after the cooldown, then it gives up.
end=$((SECONDS + 60))
while [ "$SECONDS" -lt "$end" ] && ! r_log_has 'routine nudge: pane fresh'; do sleep 1; done
r_check "the log has the nudge of the new front" r_log_has 'routine nudge: pane fresh'
end=$((SECONDS + 60))
while [ "$SECONDS" -lt "$end" ] && ! r_log_has 'routine gave up: pane fresh'; do sleep 1; done
r_check "fresh: the new front's routine gave up after its nudges" r_log_has 'routine gave up: pane fresh'
fresh_lines="$(r_file_lines "$fresh_file")"
r_check "fresh: the new front nudged it at least once and at most twice" test "$fresh_lines" -ge 1 -a "$fresh_lines" -le 2
# Give a routine that had lost its counters time to nudge again: idle 10 s plus two checks.
while [ $((SECONDS - restart_at)) -lt 30 ]; do sleep 1; done
r_check "giveup: still one line, so it stayed given up" test "$(r_file_lines "$giveup_file")" = 1
r_check "no pane was dropped for a missing profile" test "$(grep -c 'its profile is gone' "$R_LOG")" = 0

# Remove the routine block of the giveup profile; the next front stops watching that pane.
python3 - "$R_HOME/profiles.json" <<'PY'
import json, sys
path = sys.argv[1]
doc = json.load(open(path))
for p in doc["profiles"]:
    if p["name"] == "routine-giveup":
        p.pop("routine", None)
json.dump(doc, open(path, "w"), indent=2)
PY
front="$(r_front_pid)"
kill -USR2 "$holder"
r_check "a new front serves MCP after the profile edit" r_wait_new_front "$front" 30
restart_at=$SECONDS
r_check "the front logged that it no longer watches giveup" r_log_has 'routine: pane giveup: its profile is gone or has no routine now'
# The fresh pane had given up before this restart: a front that lost the counters would nudge
# it again after idle 10 s plus two checks.
while [ $((SECONDS - restart_at)) -lt 30 ]; do sleep 1; done
r_check "fresh: still the same lines after the second restart, so it stayed given up" test "$(r_file_lines "$fresh_file")" = "$fresh_lines"
r_finish
