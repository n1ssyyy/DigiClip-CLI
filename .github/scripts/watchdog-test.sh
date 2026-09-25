#!/usr/bin/env bash
# `digiclip --serve` must live exactly as long as the process that spawned
# it: a dead *grandparent* is irrelevant (2.1.0 on Windows exited 2s after
# launch because Explorer's parent is always gone), a dead *parent* means
# the shell crashed and the engine must not linger.
#
#   watchdog-test.sh <digiclip-binary>
set -uo pipefail
exe="$1"
t=$(mktemp -d)
fail() { echo "::error::$*"; cat "$t/engine.log" 2>/dev/null; exit 1; }

# The subshell exits right away, orphaning `bash -c` (the engine's parent).
( bash -c "'$exe' --serve --port 4998 --token t >'$t/engine.log' 2>&1 & echo \$! >'$t/engine.pid'; wait" &
  echo $! >"$t/parent.pid" )
for _ in $(seq 100); do
    grep -q DIGICLIP_SERVE "$t/engine.log" 2>/dev/null && break
    sleep 0.2
done
grep -q DIGICLIP_SERVE "$t/engine.log" || fail "engine never printed its serve banner"
epid=$(cat "$t/engine.pid")
ppid=$(cat "$t/parent.pid")

sleep 5
kill -0 "$epid" 2>/dev/null || fail "engine exited while its parent was alive"
echo "ok: engine alive with its grandparent gone"

kill "$ppid"
for _ in $(seq 50); do
    kill -0 "$epid" 2>/dev/null || break
    sleep 0.2
done
if kill -0 "$epid" 2>/dev/null; then
    kill -9 "$epid"
    fail "engine outlived its parent"
fi
echo "ok: engine exited after its parent died"
