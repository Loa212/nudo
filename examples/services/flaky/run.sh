#!/bin/bash
# A service that starts cleanly and then never becomes ready.
#
# This is the case that separates a deploy tool from a `systemctl restart` in a
# loop: systemd reports the unit active, because the process is running and has
# not exited. Only a health check notices that it is not actually serving.
#
# Deploy this over a healthy release and nudo will fail the health check and put
# the previous release back. `make example-break` does exactly that.
#
# Set HEALTHY=1 in the service's env to make the same script behave, which is
# how the demo deploys a good release first.

set -euo pipefail

if [ "${HEALTHY:-0}" = "1" ]; then
    echo "flaky ${APP_VERSION:-v1} starting — and this one works"
    printf '%s' "${APP_VERSION:-v1}" > /run/flaky-ready
    trap 'rm -f /run/flaky-ready' EXIT
else
    echo "flaky ${APP_VERSION:-broken} starting — and this one will never be ready"
    # Explicitly clear it, so a previous healthy release's marker cannot make
    # this one look fine.
    rm -f /run/flaky-ready
fi

# Stays up either way. That is the whole point: the process is alive, systemd is
# satisfied, and only the health check can tell the difference.
while true; do
    sleep 3600
done
