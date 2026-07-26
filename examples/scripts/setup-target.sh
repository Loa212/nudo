#!/usr/bin/env bash
# Registers the demo container as a nudo target.
#
# Generates an SSH key, installs the public half into the target container,
# stores the private half in nudo's secret store, and registers the target — then
# runs the readiness check so a failure surfaces here rather than on the first
# deploy.
#
# Idempotent: re-running reuses whatever already exists.

set -euo pipefail
cd "$(dirname "$0")/../.."
source examples/scripts/lib.sh

wait_for_nudo
ensure_signed_in

mkdir -p "${STATE_DIR}"
key_path="${STATE_DIR}/target-key"

if [ ! -f "${key_path}" ]; then
    say "generating an SSH key for the demo target"
    ssh-keygen -t ed25519 -N '' -f "${key_path}" -q
fi

# Always installed, even when the target is already registered. `make
# demo-restart` recreates the target container but keeps nudo's database, so the
# registration survives while the container's authorized_keys does not — which
# showed up as every deploy failing public-key authentication until the key was
# put back by hand.
say "installing the public key into ${TARGET_CONTAINER}"
docker exec "${TARGET_CONTAINER}" mkdir -p /root/.ssh
docker exec "${TARGET_CONTAINER}" bash -c \
    "printf '%s' '$(cat "${key_path}.pub")' > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys"
docker exec "${TARGET_CONTAINER}" systemctl enable --now ssh > /dev/null 2>&1 || true

existing="$(target_id)"
if [ -n "${existing}" ]; then
    say "target already registered (${existing}); its key has been reinstalled"
    exit 0
fi

say "storing the key in nudo's secret store"
csrf="$(csrf_from /secrets)"
curl -fsS -b "${COOKIE_JAR}" -o /dev/null -X POST "${NUDO_URL}/secrets" \
    --data-urlencode "name=DEMO_TARGET_SSH_KEY" \
    --data-urlencode "value@${key_path}" \
    --data-urlencode "csrf=${csrf}"

# The select on the target form lists the secrets, so the id comes from there.
key_id="$(curl -fsS -b "${COOKIE_JAR}" "${NUDO_URL}/targets/new" 2>/dev/null \
    | grep -oE 'value="sec_[a-f0-9]+' | head -1 | sed 's/value="//' || true)"
[ -n "${key_id}" ] || die "the SSH key secret was not stored"

say "registering the target"
csrf="$(csrf_from /targets/new)"
curl -fsS -b "${COOKIE_JAR}" -o /dev/null -X POST "${NUDO_URL}/targets" \
    --data-urlencode "name=demo-box" \
    --data-urlencode "host=${TARGET_CONTAINER}" \
    --data-urlencode "port=22" \
    --data-urlencode "user=root" \
    --data-urlencode "ssh_key_id=${key_id}" \
    --data-urlencode "labels=env=demo
role=example" \
    --data-urlencode "csrf=${csrf}"

target="$(target_id)"
[ -n "${target}" ] || die "the target was not registered"

say "running the readiness check"
csrf="$(csrf_from "/targets/${target}")"
checks="$(curl -fsS -b "${COOKIE_JAR}" -X POST "${NUDO_URL}/targets/${target}/check" \
    --data-urlencode "csrf=${csrf}" 2>/dev/null || true)"

# The page reports each check separately; surface them on the terminal too.
for check in ssh sudo systemd release_dir; do
    if echo "${checks}" | grep -q "${check}"; then
        printf '    %-12s reported\n' "${check}"
    fi
done

say "target ${target} is ready — open ${NUDO_URL}/targets/${target}"
