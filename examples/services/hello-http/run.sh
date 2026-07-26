#!/bin/bash
# A minimal HTTP service, for exercising nudo's HTTP health check.
#
# Serves its version string on :9000 and nothing else. Deliberately dependency
# free — no Python, no netcat — because the point is to exercise the deploy
# mechanics on a bare host, not to be a realistic web server.
#
# The version comes from the APP_VERSION environment variable, which nudo sets
# from the service's `env`, so redeploying with a different value visibly changes
# what the service serves.

set -euo pipefail

VERSION="${APP_VERSION:-v1}"
PORT="${PORT:-9000}"

echo "hello-http ${VERSION} starting on :${PORT}"

# A readiness marker the command health check can read, written before the
# listener comes up so the two checks can be compared.
printf '%s' "${VERSION}" > /run/hello-http-ready
trap 'rm -f /run/hello-http-ready' EXIT

# bash's /dev/tcp cannot listen, so the loop uses whichever of these the host
# has. Every image nudo is likely to target has at least one.
serve_once() {
    local body="hello-http ${VERSION}"
    local response="HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: ${#body}\r\nConnection: close\r\n\r\n${body}"

    if command -v nc > /dev/null 2>&1; then
        printf "%b" "${response}" | nc -l -p "${PORT}" -q 0 > /dev/null 2>&1
    elif command -v python3 > /dev/null 2>&1; then
        python3 - "${PORT}" "${body}" <<'PY'
import socket, sys
port, body = int(sys.argv[1]), sys.argv[2].encode()
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", port))
srv.listen(1)
conn, _ = srv.accept()
conn.recv(4096)
conn.sendall(
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
    b"Content-Length: " + str(len(body)).encode() + b"\r\n"
    b"Connection: close\r\n\r\n" + body
)
conn.close()
srv.close()
PY
    else
        # Nothing to listen with: stay up so the systemd check still passes, and
        # say why the HTTP check will not.
        echo "no nc or python3 on this host; the HTTP health check will fail" >&2
        sleep 3600
    fi
}

while true; do
    serve_once || sleep 0.2
done
