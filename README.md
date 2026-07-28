# nudo

[![release](https://img.shields.io/github/v/release/Loa212/nudo?sort=semver)](https://github.com/Loa212/nudo/releases)
[![CI](https://github.com/Loa212/nudo/actions/workflows/test.yml/badge.svg)](https://github.com/Loa212/nudo/actions/workflows/test.yml)
[![sponsor](https://img.shields.io/badge/sponsor-%E2%9D%A4-db61a2)](https://github.com/sponsors/Loa212)

A self-hostable control plane for deploying **bare-metal binaries**. The Coolify
experience — dashboard, GitHub CD, live logs, terminal, secrets — with SSH and
systemd as the deployment backend instead of Docker.

Some workloads cannot tolerate Docker's network stack overhead or its scheduler
jitter: a trading system, a latency-sensitive service. Those run as plain
systemd-managed processes on a clean host. nudo gives them the CD and
observability story that, until now, only containerised workloads had.

```
┌──────────────────────────────┐          ┌─────────────────────┐
│  control plane               │   ssh    │  target             │
│                              │ ───────► │                     │
│  nudo-server   gRPC API      │          │  OS + systemd       │
│  nudo-web      dashboard     │          │  your binary        │
│  nudo-mcp      agent tools   │          │  ...and nothing     │
│  nudo          CLI           │          │     else            │
└──────────────┬───────────────┘          └─────────────────────┘
               │ ssh (optional)
               ▼
┌──────────────────────────────┐
│  build host                  │   Where a build runs, when it
│  clone, build, send the      │   does not run on the control
│  binary back                 │   plane. Never the target.
└──────────────────────────────┘
```

**No agent is installed on a target.** Everything — probing, writing units,
uploading releases, reading the journal, opening a shell — is an SSH channel
opened from the control plane. A target runs the OS, systemd, and the binary you
deployed.

---

## How a deploy works

1. Build from a connected GitHub repo, fetch a prebuilt artifact, or take a
   binary pushed by the CLI. Builds run on the control plane by default, or on a
   [build host](#build-hosts) — **never on the target**.
2. Upload into `<release_root>/.staging-<id>/`, verify the transferred size, then
   move it to `<release_root>/releases/<id>/`. A truncated upload can never end up
   somewhere the live symlink could point.
3. Write the systemd unit and, if the service has secrets, an `EnvironmentFile`
   with mode `0600` owned by the service user.
4. Swap `<release_root>/current` to the new release with an atomic rename,
   `daemon-reload`, `enable`, `restart`.
5. Health-check: an HTTP GET, a command on the target, or `systemctl is-active`.
6. **If the check fails, put the symlink back and restart again.** The previous
   release is untouched until the new one is proven healthy, so rollback is always
   available — and it is the same operation as activation.

The unit's `ExecStart` points at `current`, never at a release directory, so the
unit file does not change between deploys.

---

## Install

### Docker

```sh
docker run -d --name nudo \
  -p 3000:3000 \
  -e NUDO_SECRET_KEY="$(openssl rand -hex 32)" \
  -e NUDO_BASE_URL="http://localhost:3000" \
  -v nudo-state:/var/lib/nudo \
  ghcr.io/loa212/nudo:latest
```

Or with compose:

```sh
NUDO_SECRET_KEY=$(openssl rand -hex 32) docker compose up -d
```

> **Keep the secret key.** It encrypts every stored secret, including your
> targets' SSH keys. Losing it makes them unrecoverable. The `nudo-state` volume
> holds the database — without it, everything is lost when the container is
> replaced.

### Binaries

Download the archive for your platform from the
[releases](https://github.com/Loa212/nudo/releases) page and verify it against its
`.sha256`. The `musl` build is fully static and runs anywhere.

```sh
tar -xzf nudo-v0.1.0-x86_64-unknown-linux-musl.tar.gz
sudo install nudo-*/nudo* /usr/local/bin/
```

To run the control plane itself as a systemd unit — which is a reasonable thing to
want from a tool like this — see [`packaging/nudo.service`](packaging/nudo.service).
It includes the setup commands and a hardened unit.

### From source

Needs a recent stable Rust (edition 2024) and `protoc`.

```sh
cargo build --release
```

---

## Quickstart: your first deploy

Open <http://localhost:3000> and create the first account. Then:

**1. Add your SSH key as a secret.** nudo needs a key that can reach your target.
Under **Secrets → Add**, paste the private key. It is encrypted immediately and
never readable again through the API or the UI.

```sh
# or from the CLI, which reads from stdin so it stays out of your shell history
nudo secrets set DEPLOY_KEY < ~/.ssh/id_ed25519
```

**2. Add the target.** Under **Targets → Add**, give it a name, a host, the SSH
user, and select the key you just stored. Then hit **Run checks** — it verifies
the host key, SSH, sudo, systemd, and a writable release directory *separately*,
so if something is wrong you find out which thing. The first successful
connection also pins the target's SSH host key; see
[Host keys](#host-keys) below.

```sh
nudo targets add edge-1 --host 10.0.0.5 --user root --ssh-key sec_abc123
nudo targets check tgt_abc123
```

**3. Add the service.** Under **Services → Add**, pick the target, name it, and
set what it runs. This is where the latency knobs live — `CPUAffinity`, `Nice`,
`IOSchedulingClass`, plus arbitrary extra unit directives. Use **View unit** to
see exactly the file a deploy would write, before writing it.

**4. Deploy.**

```sh
# push a locally built binary
nudo deploy svc_abc123 --artifact ./target/release/bot --wait

# fetch a prebuilt one — a release asset, an S3 object
nudo deploy svc_abc123 --artifact-url https://example.com/bot --wait

# or, for a git-backed service, build from a ref
nudo deploy svc_abc123 --git-ref main --wait
```

`--wait` streams progress and **exits non-zero if the deploy fails**, which is
what makes it usable as a CI step.

`--artifact` serves the file to the control plane over a short-lived loopback
listener at an unguessable path, for the length of the deploy — so the binary is
streamed rather than staged anywhere, and it needs `--wait` because this process
is what serves it. Use `--artifact-url` when the control plane is on another host
and can fetch the binary itself.

**5. Watch it.**

```sh
nudo services status svc_abc123
nudo logs svc_abc123 --follow
nudo terminal tgt_abc123
```

---

## Marking a host latency-critical

This is the feature the tool exists for. A target flagged `latency_critical`
refuses every mutating operation unless the request explicitly opts in:

```sh
nudo targets add hft-box --host 10.0.0.9 --ssh-key sec_abc --latency-critical

# refused
nudo deploy svc_hft
# error: target hft-box is marked latency-critical; set allow_latency_critical
#        on the request to mutate it

# deliberate
nudo --allow-latency-critical deploy svc_hft --wait
```

The guardrail applies to **everything**: the dashboard, the CLI, an MCP agent, and
a GitHub push. A push to a branch wired to such a target does not deploy — it is
refused and recorded. Refusals are audited, so you can see when an agent tried.

Read-only operations (`targets check`, `logs`, `services status`) are always
allowed: the host you most want to inspect should not be the one you cannot. A
changed host key is the one exception — see below.

---

## Host keys

nudo pins a target's SSH host key on the first successful connection and verifies
it on every connection after that. This matters because nudo holds the private
key for each target it manages: without verification, anything that can answer
for a target's address gets an authentication attempt with that key.

A mismatch fails closed, before authentication, with both fingerprints:

```
the host key for 10.0.0.5:22 has changed — refusing to connect.
pinned: SHA256:YDKOP3XHL0…; presented: SHA256:YWnilawiH+…
```

Unlike the latency-critical guardrail, this blocks **read-only operations too** —
logs and checks included. A mismatch may mean it is not that host at all, and
reading logs from the wrong machine is its own problem.

A rebuilt host legitimately has a new key. Review it and accept it:

```sh
nudo targets host-key tgt_abc123                              # show what changed
nudo targets host-key tgt_abc123 --accept SHA256:YWnilawiH+…  # accept it
```

or from the target's page in the dashboard, which shows both fingerprints and the
key itself. Acceptances are audited with the fingerprint and who accepted it.

Verify on the machine itself before accepting, over a console or some channel
that does not depend on that address being the right host:

```sh
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
```

`ssh-keyscan` asks the same address nudo is being redirected away from, so it
confirms nothing on its own.

For a host rebuilt while nobody was watching, whose old key is of no further
interest, `nudo targets host-key <id> --forget` reopens the first-use window. It
is the weaker option — prefer `--accept` whenever there is a fingerprint to
compare against.

Targets that existed before this was added have no pinned key, so their next
connection records one rather than refusing. Upgrading does not break a working
fleet.

---

## Build hosts

Builds run on the control plane by default, and for most instances that is the
right answer. But the control plane is often the smallest machine in the
deployment — a dashboard, a SQLite file and an SSH client, comfortable on 1 vCPU
— and pointing a service at a Rust repo makes that box run `cargo build
--release`. A build host is somewhere else to run it.

A build host is **not** a deploy target. A target runs the OS, systemd and the
binary you deployed; nothing is installed or supervised on a build host, and
nothing is ever deployed to one. They are separate things with separate
commands, and pointing a service's build at its own deploy target is not
possible.

```sh
nudo build-hosts add builder-1 --host 10.0.0.9 --user build --ssh-key sec_abc123
nudo build-hosts check bh_abc123
```

`check` verifies each prerequisite separately, as `targets check` does: the host
key, SSH, a writable workspace, and git. Not sudo, and not systemd — a build
host needs neither.

Then say where a service builds. A service's own setting always wins; without
one it follows the instance default:

```sh
nudo build-hosts default bh_abc123   # everything unpinned builds here
nudo build-hosts default --local     # ...back on the control plane
nudo build-hosts default             # show the current default
```

In the dashboard, a service's **Build on** field offers the instance default,
the control plane, or a named build host. Pinning a service to the control plane
is not the same as leaving it unset: a pinned service stays there when the
instance default later changes.

The deploy log is identical wherever a build ran — same lines, same order, same
secret redaction — so nothing downstream has to care.

**An instance that upgrades and configures nothing keeps building exactly where
it built before.** The local path is unchanged and remains the default.

### What a build host is not

**It is not a sandbox.** Builds on one host are not isolated from each other,
and nudo does not try to isolate them. A build command is arbitrary code; two
mutually distrusting builds on one machine can see each other. If that matters,
run the host so it cannot happen — a one-shot container, an ephemeral VM, a
fresh instance per build. That is an operational decision, and nudo does not
make it for you.

**Credentials do reach the build host.** nudo clones there rather than
transferring a tree from the control plane, so the host needs access to the
repository: a deploy key is written to a `0600` file for the clone's lifetime
and removed with the workspace, and an App token is passed on the command line
and redacted from any output. Register a build host as deliberately as you would
a target — its SSH host key is pinned and verified on exactly the same terms,
for exactly this reason.

**A build workspace is temporary.** Each build gets a fresh directory under the
host's workspace root, removed when the build finishes however it finishes.
There is no build cache and no shared artifact store; each build is independent.

### Latency-critical build hosts

A build host can be marked latency-critical, and building on one is allowed —
you may have exactly one spare machine. It is not silent: the dashboard, the
CLI and `check` all say that a build here will contend with whatever else runs
on the box for CPU, cache and memory bandwidth, and mutating it needs
`--allow-latency-critical` like any other latency-critical host.

Expect jitter on anything sensitive while a build is in flight. If that is not
acceptable, that machine should not be a build host.

---

## GitHub CD

Under **Sources**, create a GitHub App. nudo generates the manifest, hands your
browser off to GitHub, and receives the App id, private key and webhook secret at
the callback. Then install the App on your repositories.

Point a service at a repo and branch, tick **auto-deploy on push**, and a push
builds and deploys it. Deploy outcomes are written back as commit statuses under
the `nudo/deploy` context, so they show up on the commit and in branch protection.

A push whose every commit message contains `[skip ci]` or `[skip cd]` does not
deploy; one real commit among them does.

Webhook signatures are verified with HMAC-SHA256 over the raw request body, with a
timing-safe comparison, in every environment. There is no bypass.

For repositories without an App, add a deploy-key source instead and paste the
public half into GitHub.

---

## Agents (MCP)

`nudo-mcp` speaks MCP over stdio and exposes nine coarse-grained tools:
`list_targets`, `list_build_hosts`, `list_services`, `get_unit_status`,
`deploy`, `rollback`, `stream_logs`, `run_command`, `list_deployments`.

```json
{
  "mcpServers": {
    "nudo": {
      "command": "nudo-mcp",
      "env": {
        "NUDO_ENDPOINT": "http://127.0.0.1:50051",
        "NUDO_AGENT_LABEL": "claude"
      }
    }
  }
}
```

`deploy`, `rollback` and `run_command` **default to `dry_run: true`**, so a
mistaken call describes a plan instead of acting. Every call is attributed to the
agent session in the audit log.

There is no tool to create or edit targets, services or secrets, and no
interactive shell — those are left to a human. `run_command` (one-shot, captured)
is the shape an agent can actually use.

---

## Configuration

Every setting is a flag or an environment variable. The flags are
`--kebab-case` versions of the names below.

| Variable | Default | What it does |
|---|---|---|
| `NUDO_SECRET_KEY` | *generated* | 32-byte AES-256-GCM key, hex or base64. **Set this and keep it.** If unset, one is generated into the data directory and you are warned. |
| `NUDO_SECRET_KEY_FILE` | — | Read the key from a file instead. Preferred: it keeps the key out of the process environment. |
| `NUDO_DB` | `nudo.db` | The SQLite file. Created, with migrations applied, on startup. |
| `NUDO_DATA_DIR` | `./data` | Build workspaces, uploads, and the generated key. |
| `NUDO_BASE_URL` | `http://localhost:3000` | This instance's public URL. Decides whether the session cookie is marked `Secure`, and is what GitHub is told to call. **Set it to your real `https://` URL behind a proxy.** |
| `NUDO_WEB_ADDR` | `127.0.0.1:3000` | Where the dashboard listens. |
| `NUDO_GRPC_ADDR` | `127.0.0.1:50051` | Where the gRPC API listens. Loopback by default. |
| `NUDO_ENDPOINT` | `http://127.0.0.1:50051` | Where clients (CLI, MCP, dashboard) find the API. |
| `NUDO_TOKEN` | — | An API token. Used by the CLI, and by the dashboard when the API requires one. |
| `NUDO_REQUIRE_API_TOKEN` | `false` | Require a valid API token on every gRPC call. Off by default because the API binds to loopback with the dashboard in front of it; turn it on when anything else can reach it. The all-in-one provisions a token for its own dashboard, so enabling it cannot lock you out. |
| `NUDO_LOG_BUFFER` | `2000` | Log lines retained per service, so a freshly opened log view is not empty. |
| `NUDO_PROBE_INTERVAL` | `60` | Seconds between target reachability probes. `0` disables. |
| `NUDO_ALLOW_SETUP` | `true` | Whether first-run setup may create the initial admin. Closes automatically once an account exists. |
| `NUDO_CHECK_FOR_UPDATES` | `true` | Fetch the release manifest and show a banner when a newer version exists. The only request nudo makes on its own behalf, and it sends nothing about your instance. Can also be turned off from **Settings → This instance**. |
| `NUDO_UPDATE_MANIFEST_URL` | the repo's `releases.json` | Where the manifest is fetched from. Point it at your own copy to check against an internal mirror. |
| `NUDO_UPDATE_INTERVAL_HOURS` | `24` | Hours between checks. |
| `RUST_LOG` | `info` | Log filter. |

### Per-service configuration

Set on the service, in the dashboard or over the API:

- **Artifact source** — a URL, a git repo + branch + build command + artifact
  path, or a CLI upload.
- **Build host** — where a git build runs: the instance default, the control
  plane, or a named [build host](#build-hosts). Never the deploy target.
- **Unit** — description, exec args, working directory, user, group, restart
  policy, ordering, and the latency knobs (`CPUAffinity`, `Nice`,
  `IOSchedulingClass`) plus arbitrary extra directives written verbatim.
- **Health check** — HTTP URL, command, or `systemctl is-active`, with timeout,
  retries and initial delay.
- **`release_root`** — defaults to `/opt/<name>`.
- **`keep_releases`** — retained releases available for rollback. Default 5.
- **Secrets and env** — secret ids resolved at deploy time into the
  `EnvironmentFile`; non-secret env inlined into the unit.

---

## Running the halves separately

`nudo-all-in-one` runs the control plane and dashboard in one process, which is
what most people want. They can also run apart — the dashboard is a gRPC client,
so it does not care where the API is:

```sh
nudo-server --grpc-addr 0.0.0.0:50051 --database /var/lib/nudo/nudo.db
nudo-web    --addr 0.0.0.0:3000 --grpc-endpoint http://control-plane:50051
```

The web tier shares the database and the secret key with the server, for login
sessions and the webhook receiver.

---

## Updates and telemetry

nudo checks for new releases by fetching a static JSON file
(`releases.json` in this repository) and comparing versions. It sends **nothing**
— not your version, not an identifier, not an install count. There is no
telemetry in this codebase and no setting to disable it, because there is nothing
to disable. A test asserts that the module has no network client, so that stays
true.

Nothing is ever installed automatically. The banner links to **Upgrading nudo**
(`/upgrade`), which prints the exact commands for the way this instance is
actually installed — detected, not configured, so a container is told to pull an
image and a host install is told to verify a checksum and replace binaries.
Neither is shown the other's instructions.

Upgrading replaces executables and touches nothing else: the database, the data
directory and your configuration all live outside them, and schema changes are
applied automatically the first time the new version opens the database. The page
says this, along with the one real trap — if you never set `NUDO_SECRET_KEY`, the
generated key lives in the data directory, and every stored secret is unreadable
without it.

This is the one place nudo deliberately does less than the tools it borrowed
from: an updater that downloads a script and runs it as root is a large amount of
trust to place in a URL, for a tool that holds every target's SSH keys. A test
asserts the upgrade page has no form and pipes nothing into a shell.

Release notes are rendered in-app under **What's new**, from the manifest the last
check recorded — so an instance that has lost network access still shows the
notes for the release it knows about. Both the check and the occasional
"support this project" note can be turned off in **Settings → This instance**.

---

## Development

```sh
make help                       # every command, with what it does
make check                      # fmt, clippy, and the full test suite
make demo                       # nudo + a systemd target + three services
```

Or directly:

```sh
cargo test --workspace          # 862 unit and integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

### The demo

`make demo` starts two containers — nudo, and a Debian host running systemd as
PID 1 — registers the second as a target over SSH, and deploys the example
services in `examples/services/`. That second container is the point: without a
real systemd host you can look at the dashboard but not actually deploy anything.

```sh
make demo                       # the whole thing, from nothing
make demo-open                  # the URL and the demo credentials
make example-break              # ship a broken release and watch it roll back
make example-unit EXAMPLE=latency-critical   # the unit file a deploy writes
make demo-units                 # what systemd actually applied on the target
make demo-changelog             # seed a pretend release, to see the update banner
make demo-down                  # stop it; demo-clean also deletes the data
```

Each example under `examples/services/` is a `run.sh` and a `service.json`, and
exercises something specific: `hello-http` an HTTP health check,
`latency-critical` every scheduling knob, and `flaky` a service that starts but
never becomes ready — which is what makes the rollback demo real rather than
simulated.

The end-to-end tests need Docker. They start a systemd-enabled container, install
an SSH key into it, and deploy a real artifact through the real engine — then
make a health check fail and assert the rollback restored a *working* service.
The build-host half of the suite clones from a git repository seeded inside the
container, builds there, and asserts the binary reached the target, that the
deploy log does not say where the build ran, and that the workspace is gone
afterwards — on the failing path as well as the succeeding one. It also pushes a
second commit whose build produces a service that never becomes ready, and
asserts the rollback restored the *previously built* release and that it is
serving again:

```sh
cargo test -p nudo-server --features e2e --test e2e -- --test-threads=1
```

### Layout

```
controlplane.proto        the API contract — authoritative, unmodified
crates/proto/             generated server and client stubs
crates/format/            shared operator-facing vocabulary and formatting
crates/server/            gRPC services, deploy engine, SSH executor, SQLite
crates/web/               dashboard: axum + maud + htmx, a gRPC client
crates/cli/               nudo — a gRPC client, no duplicated logic
crates/mcp/               MCP server for agents
crates/allinone/          server + dashboard in one process
packaging/nudo.service    run the control plane itself under systemd
examples/services/        demo services, one directory each
examples/scripts/         what `make demo` runs
releases.json             the release manifest a running instance fetches
scripts/add-release.py    adds an entry to it; run by the release workflow
cliff.toml                changelog generation from the commit log
```

`CHANGES.md` records the design decisions, the bugs testing found, and what is
deliberately out of scope.

---

## Supporting nudo

nudo is free and built by one person. If it is saving you the cost of a
platform, [sponsoring](https://github.com/sponsors/Loa212) keeps it maintained.

If money is not on the table, these help as much:

- **Star the repository** — it is most of how anyone finds a project like this
- **Report what breaks** — a [bug report](https://github.com/Loa212/nudo/issues/new/choose)
  with the version, the target's OS and what you expected is worth more than a
  vague one, and more than silence
- **Say what you deployed with it** — [Discussions](https://github.com/Loa212/nudo/discussions)
  is the place; knowing what people actually run decides what gets built next

The dashboard mentions this too, at most once a month and never before you have
deployed something. It can be turned off for good in **Settings → This
instance**, and there is no telemetry to turn off, because there is none.

---

## License

MIT. Portions of the GitHub integration and the terminal design are ported from
[Coolify](https://github.com/coollabsio/coolify) (Apache 2.0) — see
[`NOTICE`](NOTICE) for what was taken and where. Vendored front-end assets keep
their own licenses; see
[`crates/web/src/assets/README.md`](crates/web/src/assets/README.md).
