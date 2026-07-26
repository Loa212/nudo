#!/bin/bash
# A stand-in for the workload nudo exists for: something that cannot tolerate a
# container runtime in its scheduling path.
#
# It does no real work. What it does is report the scheduling properties systemd
# actually applied — CPU affinity, nice level, IO class — so you can see in the
# log viewer that the latency knobs on the service definition took effect on the
# host, rather than trusting that they were written.

set -euo pipefail

echo "latency-critical worker starting (pid $$)"

# What systemd actually gave us, as opposed to what the unit asked for.
if command -v taskset > /dev/null 2>&1; then
    echo "cpu affinity: $(taskset -cp $$ 2>/dev/null | sed 's/.*: //')"
else
    echo "cpu affinity: (taskset not installed on this host)"
fi

echo "nice level:   $(ps -o ni= -p $$ | tr -d ' ')"

if command -v ionice > /dev/null 2>&1; then
    echo "io class:     $(ionice -p $$ 2>/dev/null)"
else
    echo "io class:     (ionice not installed on this host)"
fi

# A secret, if one is attached. Printed as a digest rather than a value, because
# a service that logs its own credentials is a bad example to ship.
if [ -n "${API_TOKEN:-}" ]; then
    if command -v sha256sum > /dev/null 2>&1; then
        echo "API_TOKEN:    present, sha256 $(printf '%s' "${API_TOKEN}" | sha256sum | cut -c1-12)"
    else
        echo "API_TOKEN:    present (${#API_TOKEN} characters)"
    fi
else
    echo "API_TOKEN:    not set"
fi

printf '%s' ready > /run/latency-critical-ready
trap 'rm -f /run/latency-critical-ready' EXIT

# A tight-ish loop that logs a heartbeat, so the live log view has something to
# show and you can watch it stream.
tick=0
while true; do
    tick=$((tick + 1))
    echo "tick ${tick} — still on cpu $(ps -o psr= -p $$ | tr -d ' ')"
    sleep 5
done
