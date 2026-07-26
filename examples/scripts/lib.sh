#!/usr/bin/env bash
# Shared helpers for the example scripts.
#
# The dashboard is driven over HTTP rather than the CLI for the setup steps,
# because service definitions are created through the API and the CLI has no
# verb for that (see CHANGES.md). Deploys use the CLI, which is what a person
# would actually run.

set -euo pipefail

# Where the demo instance lives. Overridable so the scripts also work against a
# nudo running outside the demo compose file.
NUDO_URL="${NUDO_URL:-http://127.0.0.1:3000}"
NUDO_GRPC="${NUDO_ENDPOINT:-http://127.0.0.1:50051}"

# The demo account. Not a secret: this is a throwaway local instance.
DEMO_EMAIL="${DEMO_EMAIL:-admin@example.com}"
DEMO_PASSWORD="${DEMO_PASSWORD:-correct horse battery staple}"

# State the scripts share between invocations.
STATE_DIR="${STATE_DIR:-.nudo-demo}"
COOKIE_JAR="${STATE_DIR}/cookies.txt"

# The container names the Makefile uses.
NUDO_CONTAINER="${NUDO_CONTAINER:-nudo-demo}"
TARGET_CONTAINER="${TARGET_CONTAINER:-nudo-demo-target}"

# How the control plane reaches artifacts served from this machine.
#
# nudo runs in a container, so `127.0.0.1` in an artifact URL means the container
# itself — not the host. Docker Desktop resolves `host.docker.internal` to the
# host; on Linux the Makefile adds it via `--add-host`.
ARTIFACT_HOST="${ARTIFACT_HOST:-host.docker.internal}"
ARTIFACT_PORT="${ARTIFACT_PORT:-8099}"

# The `nudo` binary. Prefers a release build, then a debug build, then $PATH.
nudo_cli() {
    local candidate
    for candidate in target/release/nudo target/debug/nudo; do
        if [ -x "${candidate}" ]; then
            echo "${candidate}"
            return
        fi
    done
    command -v nudo 2>/dev/null || {
        echo "the nudo CLI is not built; run: cargo build --release -p nudo-cli" >&2
        exit 1
    }
}

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!  \033[0m%s\n' "$*" >&2; }
die() { printf '\033[1;31mx  \033[0m%s\n' "$*" >&2; exit 1; }

# Waits for the dashboard to answer.
wait_for_nudo() {
    local i
    for i in $(seq 1 60); do
        if curl -fsS "${NUDO_URL}/login" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    die "nudo did not come up at ${NUDO_URL} — try: make demo-logs"
}

# Reads a CSRF token out of a page. Every form carries one, so any authenticated
# page will do.
csrf_from() {
    curl -fsS -b "${COOKIE_JAR}" "${NUDO_URL}$1" 2>/dev/null \
        | grep -oE 'name="csrf" value="[^"]+' \
        | head -1 \
        | sed 's/.*value="//' || true
}

# Signs in, creating the account on first run.
#
# Idempotent: a second call reuses the existing account, so the scripts can be
# run in any order.
ensure_signed_in() {
    mkdir -p "${STATE_DIR}"

    # An existing session still works?
    if [ -f "${COOKIE_JAR}" ] \
        && curl -fsS -b "${COOKIE_JAR}" -o /dev/null -w '%{http_code}' "${NUDO_URL}/targets" 2>/dev/null | grep -q 200; then
        return 0
    fi

    rm -f "${COOKIE_JAR}"

    # First run: claim the instance. Once an account exists this redirects to
    # /login instead, which the sign-in below then handles.
    curl -fsS -c "${COOKIE_JAR}" -o /dev/null -X POST "${NUDO_URL}/setup" \
        --data-urlencode "email=${DEMO_EMAIL}" \
        --data-urlencode "password=${DEMO_PASSWORD}" \
        --data-urlencode "display_name=Demo admin" \
        --data-urlencode "csrf=nudo-pre-auth" 2>/dev/null || true

    if curl -fsS -b "${COOKIE_JAR}" -o /dev/null -w '%{http_code}' "${NUDO_URL}/targets" 2>/dev/null | grep -q 200; then
        say "created the demo account (${DEMO_EMAIL})"
        return 0
    fi

    curl -fsS -c "${COOKIE_JAR}" -o /dev/null -X POST "${NUDO_URL}/login" \
        --data-urlencode "email=${DEMO_EMAIL}" \
        --data-urlencode "password=${DEMO_PASSWORD}" \
        --data-urlencode "csrf=nudo-pre-auth"

    curl -fsS -b "${COOKIE_JAR}" -o /dev/null -w '%{http_code}' "${NUDO_URL}/targets" 2>/dev/null | grep -q 200 \
        || die "could not sign in to ${NUDO_URL}"
}

# The demo target's id, or empty when it does not exist yet.
#
# `|| true` because grep exits non-zero when it matches nothing, which under
# `set -e` would abort the caller — but "no target yet" is the normal first-run
# state, not a failure.
target_id() {
    curl -fsS -b "${COOKIE_JAR}" "${NUDO_URL}/targets" 2>/dev/null \
        | grep -oE 'tgt_[a-f0-9]+' \
        | head -1 || true
}

# A service's id by name, or empty.
service_id_by_name() {
    local name="$1"
    # Same reasoning as target_id: absent is a normal answer, not an error.
    "$(nudo_cli)" --endpoint "${NUDO_GRPC}" services list --output json 2>/dev/null \
        | python3 -c "
import json, sys
try:
    services = json.load(sys.stdin)['services']
except Exception:
    sys.exit(0)
for service in services:
    if service['name'] == '$name':
        print(service['id'])
        break
" || true
}

# Serves a file to the control plane for the length of a deploy.
#
# Returns the URL to pass to \`nudo deploy --artifact-url\`. The CLI's own
# \`--artifact\` binds to loopback, which a containerised control plane cannot
# reach — so the demo publishes on all interfaces instead.
start_artifact_server() {
    local directory="$1"

    # Reuse one if it is already up, so repeated deploys do not stack servers.
    if curl -fsS -o /dev/null "http://127.0.0.1:${ARTIFACT_PORT}/" 2>/dev/null; then
        return 0
    fi

    python3 -m http.server "${ARTIFACT_PORT}" --bind 0.0.0.0 --directory "${directory}" \
        > /dev/null 2>&1 &
    echo $! > "${STATE_DIR}/artifact-server.pid"

    local i
    for i in $(seq 1 20); do
        if curl -fsS -o /dev/null "http://127.0.0.1:${ARTIFACT_PORT}/" 2>/dev/null; then
            return 0
        fi
        sleep 0.3
    done
    die "the artifact server did not start on :${ARTIFACT_PORT}"
}

stop_artifact_server() {
    if [ -f "${STATE_DIR}/artifact-server.pid" ]; then
        kill "$(cat "${STATE_DIR}/artifact-server.pid")" 2>/dev/null || true
        rm -f "${STATE_DIR}/artifact-server.pid"
    fi
}
