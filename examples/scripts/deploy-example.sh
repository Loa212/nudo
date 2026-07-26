#!/usr/bin/env bash
# Creates (if needed) and deploys one of the examples in examples/services/.
#
#   examples/scripts/deploy-example.sh hello-http
#   examples/scripts/deploy-example.sh flaky --env HEALTHY=0
#
# The service definition comes from that example's service.json; the binary is
# its run.sh, served to the control plane for the length of the deploy.

set -euo pipefail
cd "$(dirname "$0")/../.."
source examples/scripts/lib.sh

example="${1:-}"
[ -n "${example}" ] || die "usage: deploy-example.sh <example> [--env KEY=VALUE ...]"
shift || true

example_dir="examples/services/${example}"
[ -d "${example_dir}" ] || die "no such example: ${example} (see examples/services/)"
[ -f "${example_dir}/service.json" ] || die "${example_dir}/service.json is missing"
[ -f "${example_dir}/run.sh" ] || die "${example_dir}/run.sh is missing"

# Env overrides, so one example can be deployed twice with different behaviour —
# which is how the rollback demo ships a broken release of a working service.
env_override=""
while [ $# -gt 0 ]; do
    case "$1" in
        --env)
            shift
            env_override="${env_override}${env_override:+$'\n'}$1"
            ;;
        *) die "unknown argument: $1" ;;
    esac
    shift
done

wait_for_nudo
ensure_signed_in

target="$(target_id)"
[ -n "${target}" ] || die "no target registered — run: make demo-target"

service="$(service_id_by_name "${example}")"

if [ -z "${service}" ]; then
    say "creating the ${example} service"

    # service.json holds the form fields; $comment is documentation and is
    # dropped. Anything passed with --env replaces the file's env.
    form="$(python3 - "${example_dir}/service.json" "${target}" "${env_override}" <<'PY'
import json, sys, urllib.parse

path, target_id, env_override = sys.argv[1], sys.argv[2], sys.argv[3]
with open(path) as handle:
    definition = json.load(handle)

definition.pop("$comment", None)
definition["target_id"] = target_id
# Every example ships a binary from disk rather than building from git.
definition["artifact_kind"] = "upload"
if env_override:
    definition["env"] = env_override

print("&".join(
    f"{urllib.parse.quote(str(key))}={urllib.parse.quote(str(value))}"
    for key, value in definition.items()
))
PY
)"

    csrf="$(csrf_from /services/new)"
    curl -fsS -b "${COOKIE_JAR}" -o /dev/null -X POST "${NUDO_URL}/services" \
        --data "${form}" --data-urlencode "csrf=${csrf}"

    service="$(service_id_by_name "${example}")"
    [ -n "${service}" ] || die "the service was not created"
else
    say "reusing the existing ${example} service (${service})"

    if [ -n "${env_override}" ]; then
        say "applying the env override"
        form="$(python3 - "${example_dir}/service.json" "${target}" "${env_override}" <<'PY'
import json, sys, urllib.parse

path, target_id, env_override = sys.argv[1], sys.argv[2], sys.argv[3]
with open(path) as handle:
    definition = json.load(handle)

definition.pop("$comment", None)
definition["target_id"] = target_id
definition["artifact_kind"] = "upload"
definition["env"] = env_override

print("&".join(
    f"{urllib.parse.quote(str(key))}={urllib.parse.quote(str(value))}"
    for key, value in definition.items()
))
PY
)"
        csrf="$(csrf_from "/services/${service}/edit")"
        curl -fsS -b "${COOKIE_JAR}" -o /dev/null -X POST "${NUDO_URL}/services/${service}/edit" \
            --data "${form}" --data-urlencode "csrf=${csrf}"
    fi
fi

# The control plane fetches the binary over HTTP. `nudo deploy --artifact` serves
# it on loopback, which a containerised control plane cannot reach — so the demo
# publishes it on all interfaces and passes a URL the container can resolve.
artifact_dir="${STATE_DIR}/artifacts"
mkdir -p "${artifact_dir}"
cp "${example_dir}/run.sh" "${artifact_dir}/${example}"
chmod +x "${artifact_dir}/${example}"
start_artifact_server "${artifact_dir}"

say "deploying ${example}"
"$(nudo_cli)" --endpoint "${NUDO_GRPC}" deploy "${service}" \
    --artifact-url "http://${ARTIFACT_HOST}:${ARTIFACT_PORT}/${example}" \
    --wait \
    || {
        if [ "${example}" = "flaky" ]; then
            warn "the deploy failed — which is what this example is for"
        else
            warn "the deploy failed; the dashboard has the output"
        fi
        echo
        say "see it at ${NUDO_URL}/services/${service}"
        exit 1
    }

echo
say "deployed — ${NUDO_URL}/services/${service}"
