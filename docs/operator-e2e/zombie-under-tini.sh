#!/usr/bin/env bash
# Check the image under a real PID 1: no defunct process after a pane leaves a
# background process behind, SIGUSR2 reaches the holder through tini, and stopping the
# container ends the panes. Starts no MCP server: the holder runs its server in the
# `--no-listen` test form, so nothing listens.
#
#   IMAGE=<image> ./zombie-under-tini.sh            # the real entrypoint (tini): expect PASS
#   IMAGE=<image> NO_TINI=1 ./zombie-under-tini.sh  # holder as PID 1: expect FAIL (control)
#
# DOCKER defaults to `docker`; on a Mac set DOCKER to the docker binary.
set -u
IMAGE="${IMAGE:?set IMAGE to the image to check}"
DOCKER="${DOCKER:-docker}"
NAME="shepherd-zombie-$$"
fail=0
check() { # check <description> <command...>
  local what="$1"; shift
  if "$@"; then echo "PASS $what"; else echo "FAIL $what"; fail=1; fi
}
cleanup() { "$DOCKER" rm -f "$NAME" >/dev/null 2>&1; }
trap cleanup EXIT

entry=()
[ "${NO_TINI:-}" = 1 ] && entry=(--entrypoint openab-shepherd)
args=(hold -- --no-listen
  --test-pane 'orphan=sleep 300 & echo started; exit 0'
  --test-pane 'keep=exec sleep 300')
[ "${NO_TINI:-}" != 1 ] && args=(openab-shepherd "${args[@]}")

"$DOCKER" run -d --name "$NAME" ${entry[@]+"${entry[@]}"} "$IMAGE" "${args[@]}" >/dev/null || exit 2
sleep 5

defunct() { "$DOCKER" exec "$NAME" ps -eo stat=,comm= | awk '$1 ~ /^Z/' ; }
check "no defunct process after a pane left a background process behind" \
  test -z "$(defunct)"
echo "PID 1 is: $("$DOCKER" exec "$NAME" cat /proc/1/comm)"

holder() { "$DOCKER" exec "$NAME" pgrep -x openab-shepherd | head -1; }
front_of() { "$DOCKER" exec "$NAME" ps -eo pid=,ppid=,args= | awk -v h="$1" '$2==h && /front/ {print $1}' | head -1; }
h=$(holder); [ "${NO_TINI:-}" != 1 ] && h=$("$DOCKER" exec "$NAME" pgrep -P 1 -x openab-shepherd | head -1)
before=$(front_of "$h")
"$DOCKER" kill -s USR2 "$NAME" >/dev/null
sleep 3
after=$(front_of "$h")
check "SIGUSR2 to the container restarts the front (was $before, now $after)" \
  test -n "$before" -a -n "$after" -a "$before" != "$after"
check "the 'keep' pane survived SIGUSR2" \
  "$DOCKER" exec "$NAME" pgrep -f 'sleep 300'

"$DOCKER" stop -t 15 "$NAME" >/dev/null
code=$("$DOCKER" inspect -f '{{.State.ExitCode}}' "$NAME")
echo "container exit code: $code"
check "docker stop ended the container (panes ended with the holder)" \
  test "$("$DOCKER" inspect -f '{{.State.Running}}' "$NAME")" = false
exit "$fail"
