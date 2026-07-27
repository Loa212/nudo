# Changelog

Every release of nudo. Generated from the commit log by git-cliff, so it
reflects what actually landed rather than what someone remembered to write
down.

Also rendered inside the dashboard under "What's new" — see `releases.json`,
which is the manifest a running instance fetches.

## 0.1.0 — 2026-07-27

### Added

- Add the release check, changelog and support prompt

Ported from Coolify, with the parts that made sense for a tool that holds
every target's SSH keys.

The release check fetches a static JSON manifest, compares versions with a
real semver comparison, and shows a banner when something newer is out. It
sends nothing about the instance — no version, no identifier, no install
count — and it never installs anything. Coolify's equivalent downloads a
shell script from a CDN and runs it as root on the host; that trade is not
worth it here, so the banner is a link and upgrading stays a deliberate act.
A test asserts the banner offers no such thing, so that cannot change
quietly.

The changelog page renders the manifest's notes without another network
call, so an instance that has lost network access still shows the notes for
the release it knows about. Notes are untrusted input, so rather than trust
a Markdown library's sanitiser they go through a small renderer that handles
bullets and paragraphs and escapes everything else.

The support prompt keeps Coolify's monthly recurrence ("Maybe next time",
back the following month) and its instance-wide off switch, but stores the
dismissal against the user rather than in localStorage, so it is honoured on
every device rather than resetting per browser. It also waits until five
deployments have succeeded: asking for support before someone has deployed
anything is asking a stranger for money. Coolify's popup can appear on an
empty instance.

Not ported: the "still alive" ping Coolify sends home on every boot, on by
default. There is no telemetry here and no setting to disable, because there
is nothing to disable. A test scans this module for a network client so that
stays true.

Both switches are on the settings page, and the release check consults the
stored one on every tick, so unticking the box stops the next check rather
than the next restart.
- Add a Makefile and runnable examples

'make demo' brings up nudo, a real systemd target it deploys to over SSH,
and three example services — the setup I previously walked through by
hand, with the two rough edges handled properly.

The examples each demonstrate one thing: hello-http an HTTP health check,
latency-critical every scheduling knob (and it logs what systemd actually
applied, so the log viewer proves they landed), and flaky a release that
starts but never becomes ready — which systemd reports as active and only
a health check catches. 'make example-break' ships that one over a working
release and rolls back.

Two container-boundary details the scripts handle, since both bit me doing
this manually: the demo reuses one generated secret key so a restart does
not orphan every stored secret, and artifacts are served on all interfaces
rather than loopback, because 'nudo deploy --artifact' binds to 127.0.0.1
which a containerised control plane cannot reach.

Verified end to end: all three deploy, latency-critical really does get
Nice=-10, IOSchedulingClass=1 and CPUAffinity=0-1 from systemd, and the
rollback demo works.
- Add direct CLI upload for locally built binaries

PLAN.md requires deploying from a direct CLI upload and the README
documented it, but the CLI only had --artifact-url: the engine supported an
uploaded artifact with no path from the CLI to reach it. Found by driving a
real deploy through the CLI rather than by reading the code.

'nudo deploy --artifact ./target/release/bot --wait' now serves the file to
the control plane over a short-lived loopback listener at an unguessable
path, for the length of the deploy. That reuses the artifact-fetch path
rather than adding an upload RPC the proto does not define, and the binary
is streamed rather than staged anywhere. It requires --wait, since this
process is what serves the file, and says so if omitted.

Verified end to end against a systemd container: deploy v1, confirm
CPUAffinity=0-1 and Nice=-5 actually applied by systemd on the target,
journald logs streaming, exec working, deploy v2, then roll back and
confirm the target is serving v1 again.

671 tests.
- Add README and CHANGES

README: what it is and why, how a deploy actually works, install by Docker
or binary, a quickstart to a first deploy, the latency-critical guardrail,
GitHub CD, the MCP setup, and a full configuration reference.

CHANGES: no proto changes were needed — every RPC in all eight services is
implemented, including the four streams and the bidi Attach. Records the
two places the proto leaves a gap and how each is handled, the design
decisions taken where the brief left a choice, the five bugs testing found,
and what is deliberately out of scope with a reason for each.
- Add GitHub and gRPC integration tests

GitHub (20 tests, wiremock): manifest code exchange with the headers GitHub
requires, refusal of a conversion response missing the private key or
webhook secret, installation-token minting with the presented JWT decoded
to prove it is RS256 and App-signed, installation ownership verification,
paginated repo and branch listing, and commit-status writeback with the
140-character cap.

Token caching is asserted rather than assumed: five calls mint exactly one
token, a token inside the refresh margin is replaced, and an expired one is
refreshed. A malformed repository name is proven never to reach GitHub at
all.

gRPC (19 tests): a real tonic server on an ephemeral port driven by the
generated client, so the codec, the streaming plumbing and the status codes
a client actually sees are covered. Includes the guardrail refusing an
agent over the wire while auditing the attempt, idempotent retries,
dry-run deploys changing nothing, secrets having no read path, and a
terminal attach refusing both a missing attach frame and a forged token.

668 tests passing.
- Add the dashboard's rendering layer

98 render tests covering the full screen set: dashboard with aggregate
counts, targets and services lists and details, service and target forms
(including every latency knob), deployment history and the live view,
log viewer, secrets, sources, audit, terminal, settings, login and setup.

Security properties are asserted, not assumed: no secret value renders
anywhere on any page; every POST form on all twelve form-bearing screens
carries its CSRF token; log text, unit files, audit summaries, grep values
and error messages all escape HTML; the terminal page embeds its token as
JSON with '</' rewritten so a hostile token cannot close the script
element, and contains no host, port or user@ for the target.

The SSE fragments are separate functions from the pages that host them, so
a live update swaps just the log pane.

Workspace-wide: 629 tests passing.
- Add end-to-end deployment tests, packaging and CI

Three tests against a real systemd host: a Debian container with systemd
as PID 1, sshd, and a throwaway key. They deploy a real artifact through
the real engine and assert on the target itself — unit active and enabled,
the symlink pointing at the release, the binary executable, and the unit
file on disk byte-identical to what RenderUnit previews.

Running it for real found two bugs that no unit test would have:

1. SSH sends EOF *before* the exit status, so breaking the receive loop on
   Eof made every remote command report failure. The probe was reporting
   'systemd 252 (...)' as a FAILED systemd check while printing the
   correct output. Every systemctl call was affected.

2. render_env_file escaped '$' as '65091'. That is correct for a unit's
   Environment= directive, which systemd expands, but wrong for an
   EnvironmentFile, which it does not — so a secret containing '$' reached
   the service with a literal '65091'. The two contexts now have separate
   escapers, each with a test pinning the difference.

The rollback test is the one that matters most: the bad release starts and
stays active, so systemd reports success while the service never becomes
ready. A tool that only asks systemd would call that a good deploy; this
one health-checks, rolls back, and is verified healthy again afterwards.

Also: Dockerfile (one build path — the image compiles the binaries),
docker-compose for a one-command run, test and release workflows with a
static-linking check on the musl artifacts, a hardened example systemd
unit for the control plane itself, NOTICE with the Coolify attribution
and a precise account of what was ported, and MIT LICENSE.

422 unit tests, 3 end-to-end.
- Add the MCP server

A curated eight-tool surface for LLM agents — list_targets,
list_services, get_unit_status, deploy, rollback, stream_logs,
run_command, list_deployments — not a mapping of every RPC. Creating
infrastructure, editing secrets and holding a PTY are deliberately
absent, with a test that asserts they stay absent.

Agent-safety properties: deploy/rollback/run_command default to
dry_run TRUE so a mistaken call reports a plan; the latency-critical
guardrail must be opted into per call and the tool descriptions say when
that is and is not appropriate; every call is attributed to the agent
session in the audit log; log reads and command output are capped so a
tool cannot fill an agent's context.

Result shapes are purpose-built rather than raw proto: enum names instead
of wire integers, and a service summary mirrors its target's
latency-critical flag so the agent needn't cross-reference.

Verified over the real stdio protocol. Fixed a spec violation the tests
caught: MCP requires an object at the root of a tool's output schema, so
listings are wrapped rather than returned as bare arrays.

20 tests.
- Add web tier: auth, webhook receiver, terminal socket, SSE routes

Dashboard as a gRPC client of the control plane. The browser holds no
gRPC connection: live views hold the upstream stream server-side and push
rendered HTML over SSE, folding frames into 'the latest' and rendering on
a fixed tick (biased select) so a log burst cannot pin the browser.

Sessions: argon2 login, digest-only cookie storage, HttpOnly/SameSite=Lax
with Secure derived from the configured base URL (a proxy terminates TLS,
so the request cannot decide), per-session CSRF on every form, and
first-run setup that closes once an account exists.

GitHub webhooks: signature verified over the raw body before any parsing,
with no environment that skips it. A source with no webhook secret is
refused rather than trusted. The latency-critical guardrail applies to a
push exactly as to an agent. Deploy outcomes are written back as commit
statuses.

Terminal websocket: the client sends only an opaque single-use token; the
server looks the host up itself. Tagged JSON frames both ways, so output
equal to a control word cannot be misread as one — the failure mode
Coolify's raw-sentinel protocol has.

Assets (htmx, sse, xterm, fit addon) vendored and embedded, served from a
fixed match so there is no path traversal and no CDN fetch.
- Add the nudo CLI

A pure gRPC client — init, targets, services, deploy, rollback, logs,
exec, terminal, secrets, audit, sources — so no decision or guardrail is
duplicated outside the server.

Built for CI: --output json with a stable shape (enum names, not wire
integers, and no field that could ever hold a secret value),
--idempotency-key, 'deploy --wait' that streams progress and exits
non-zero when the deployment fails, and 'exec' propagating the remote
exit status. Secret values are read from stdin by default so they stay
out of shell history and the process table.

Verified end to end against a live server: secrets sealed with only a
digest returned, label selectors, and the latency-critical guardrail
refusing an unattended exec while auditing the refusal.

Pins one sharp edge with a test: 'exec' takes trailing var args, so
nudo's own global flags must precede the subcommand.

36 tests.
- Implement the full gRPC surface and the server binary

All eight proto services: Targets, ServicesApi, Deployments, Logs,
Terminals, Sources, Secrets, Audit — including the three server streams
(WatchUnitStatus, Watch, Stream) and the bidi terminal Attach.

Every mutating RPC goes through one authorize() that enforces the
latency-critical guardrail and writes the audit entry, so a new RPC
cannot forget either. Refused mutations are audited too. dry_run returns
the plan without touching anything, and Deploy honours idempotency keys.

Terminal Attach redeems a single-use token and looks the host up itself;
the client never names a host or supplies a command line.

Server binary: migrations on startup, periodic reachability probing,
terminal-grant sweeping, gRPC health reporting, graceful SIGTERM, and
reconciliation of deployments interrupted by a restart.

Two bugs the tests caught: the rm -rf guard on service delete sat after
the SSH connect so it was unreachable, and the rollback response did not
carry the release it targeted.

418 tests.
- Add deploy engine, health checks, GitHub App client and git builder

Deploy engine: stage the artifact outside releases/, verify the upload
size, move it in atomically, write unit + 0600 EnvironmentFile, swap the
'current' symlink via rename, daemon-reload/enable/restart, health-check,
and roll back by re-pointing the symlink when the check fails. Cancel is
checked between steps so it never interrupts a swap. Retention runs only
after the new release is healthy.

Health checks: HTTP (curl --fail so a 500 is unhealthy), command, or
systemctl is-active, evaluated from the target's own network position.

GitHub: manifest flow, RS256 App JWTs with back-dated iat, installation
tokens cached against GitHub's expires_at (Coolify re-mints every call),
paginated repo/branch listing, and Commit Statuses writeback. No local-env
signature bypass, unlike the reference implementation.

Git builds run on the control plane, never on a target. Tokens are passed
via 'git -c url.insteadOf' and redacted from all forwarded output;
artifact paths cannot escape the checkout.

275 tests.
- Add SQLite state layer with migrations

Schema and persistence for targets, services, releases, deployments,
secrets, sources, users/sessions/API tokens, terminal grants and audit.
Migrations run on startup; WAL so the deploy engine can write progress
while the dashboard reads.

Security properties enforced at this layer: session cookies, API tokens,
terminal tokens and GitHub setup states are stored only as sha256
digests; secrets, App private keys and webhook secrets are sealed;
setup states and terminal grants are atomically single-use; GitHub
credentials cannot be rebound once configured.

Runtime queries rather than sqlx::query! so a clean checkout builds
without a prepared database. Dynamic SQL is composed from const
fragments with bound parameters only, wrapped in AssertSqlSafe.

197 tests.
- Add crypto, systemd rendering and SSH executor

- AES-256-GCM secret sealing with per-value nonces, argon2id passwords,
  and GitHub webhook HMAC verified against GitHub's documented vector
  over the raw body with a constant-time compare.
- Systemd unit rendering that execs through the 'current' symlink so
  activation is a symlink swap; latency knobs, escape-hatch directives,
  deterministic map ordering, and injection-safe env escaping.
- Release path layout, retention and rollback-target selection.
- SSH executor: agentless exec/stream/upload/PTY over russh, with
  shell-quoting on every interpolated value and verified upload sizes.

67 tests.
- Add Cargo workspace and generated proto crate

The proto at the repo root stays the single authoritative contract; the
proto crate generates both server and client stubs from it so every
binary shares one set of types. Adds timestamp/actor/status helpers that
the rest of the workspace needs, with tests.
- Add skills

### Fixed

- Fix three things the first real release exposed

None of these could show up before, because nothing tags on an ordinary push
and the release workflow only runs on a tag.

download-artifact with no pattern also tried to fetch the .dockerbuild build
record that docker/build-push-action uploads on its own. It failed on that
one after five retries and took the publish step with it — after both real
archives had already downloaded and verified their digests. Now restricted
to the nudo-* archives this workflow uploads.

The release body told people to pull ghcr.io/Loa212/nudo:v0.1.0, which is
wrong twice: GHCR paths are lowercase, and metadata-action's semver pattern
strips the leading v, so the published tags are 0.1.0, 0.1 and latest. That
instruction was uncopyable — verified both failure modes against the real
registry before fixing.

The manifest's notes were the entire GitHub release body, so the dashboard's
"What's new" showed tar and docker run install instructions to people who are
by definition already running nudo. They now come from the generated
CHANGELOG.md section for that version. That extraction is a script rather
than inline YAML because a greedy match would splice every older release into
one entry, which is worth a test rather than a careful read — nine of them,
wired into make test-scripts.
- Fix the static-linking check that failed every correct build

The release job verified the musl binaries were static by grepping `ldd`
output for "not a dynamic executable" with `grep -qv` — which matches any
other line. A correctly static musl binary makes `ldd` print "statically
linked" instead, so the check rejected exactly the binaries it existed to
pass, and the first real release failed on it.

It now reads the ELF dynamic section for NEEDED entries, which is the
property being claimed: depends on no shared library at run time.

Not INTERP, though that was my first attempt: Rust links musl targets as
-static-pie, so a genuinely static binary still carries an interpreter
segment and that check would reject everything this job builds. Verified
against a real static musl build and a dynamic gnu one before committing.
- Correct the test count in the README
- Fix the remaining audit findings

- dry_run reported success for operations that would be refused. A dry run
  exists to say what would happen, so the refusal checks in Secrets.Delete
  and Sources.Delete now run before the dry-run return, with tests pinning
  the order.
- The log ring buffer was written and never wired: tail_into_buffer had no
  caller, so a freshly opened log view fell back to a live journalctl read
  and the buffer never warmed. Tailers now start for services being watched
  and stop when the last viewer goes, so an idle control plane holds no SSH
  connection to a latency-critical box.
- idempotency_key was honoured only by Deploy. Generalised onto Authorized
  and applied to RunCommand, which is the dangerous one: a CI retry after a
  dropped connection would otherwise run 'systemctl restart' twice. The key
  is recorded only once the command reaches the target, so a retry after a
  connection failure is still free to try.
- WatchUnitStatus was implemented but unused, leaving the services list
  frozen until reload. The dashboard now streams it over SSE on the same
  fold-fast/render-slow tick, folding one message per service into a
  snapshot so a host with many services does not repaint per service.

CHANGES.md records three further gaps as deliberately deferred with
reasons: no RPC for creating a deploy-key source, no CLI verb for defining
a service, and commit statuses only for push-triggered deploys.

691 tests.

### Other

- Publish v0.1.0

Adds the release to releases.json, which running instances fetch to
tell their operator a new version is out, and regenerates CHANGELOG.md.
Made by the release workflow.
- Download only the release archives when publishing

docker/build-push-action uploads a .dockerbuild build record of its own.
With no pattern, download-artifact tried to fetch that too, failed on it
after five retries, and took the publish step down — after both real
archives had already downloaded and verified their digests.

Restricted to the nudo-* archives this workflow uploads.
- Tell each install how to upgrade itself

The update banner said a new version existed and left it there. Now it links
to /upgrade, which prints the exact commands for the way this instance is
actually installed.

The install kind is detected rather than configured — /.dockerenv, an
environment marker set by the image, or /proc/1/cgroup — because an operator
should not have to tell nudo something it can see, and a wrong answer here
means printing instructions that do not apply. A container gets a docker pull
and recreate; a host install gets download, verify the checksum, replace the
binaries, restart the unit. Each sees only its own: telling someone in a
container to restart a systemd unit is worse than telling them nothing, and
tests assert the other kind's instructions are absent.

The page leads with what upgrading does to your data, since that is the first
question. It replaces executables and nothing else — the database, the data
directory and the configuration all live outside them, and migrations run
automatically when the new version opens the database. Verified rather than
asserted: an instance with an account and an encrypted secret went through the
full stop/rm/run cycle, and the session, the account and the secret all
survived.

It stays a page rather than a button. Coolify's updater downloads a script
and runs it as root; for a tool holding every target's SSH keys that is a
large amount of trust in a URL. Two tests keep it that way — the banner
submits nothing and contains no shell command, and the upgrade page has no
form and pipes nothing into a shell.

An up-to-date instance is shown `latest` rather than the version it is
already running, since pulling that is a no-op dressed up as an instruction.

A self-updating binary is deferred, not rejected: it needs signature
verification and the same staged swap nudo does for services pointed at
itself, and it can only ever apply to the binary install.
- Give the login page its own document, and the dashboard a first run

Two fixes to what someone sees before they have done anything.

The login and setup pages render their own document via `auth_shell`, and
`login_page`'s doc comment says as much — but all four call sites wrapped
them in `page()` as well, which adds the navigation rail. A signed-out
visitor got the full navigation beside the login form, every item of which
redirects straight back to login, inside two nested documents. Now they are
returned unwrapped, with a test asserting no rail and exactly one doctype;
the test was verified to fail against the old code.

First-run setup already worked — with no users, /login is the account
creation form instead, and it closes once an account exists — but nothing
after it said what to do next. Four zeroes and an empty deployments table
leave the order (a target before a service, a service before a deploy) to be
inferred. The dashboard now carries a three-step checklist until the first
deployment exists, with only the current step offering a button: three
buttons at once is three decisions, and the point is that there is one thing
to do next. It disappears once anything has been deployed, because a
checklist that outlives its usefulness becomes furniture.

The "No targets yet" card is suppressed while the checklist is up, since its
first step is the same request. It still appears for an instance that has
deployed before and later lost every target.
- Warn when the demo is competing with the e2e tests

The e2e suite starts its own systemd containers. With the demo also up,
Docker gets starved and the suite times out — 4½ minutes and a failure
against 45 seconds and a pass — which reads as a real regression rather
than as contention.
- Publish the changelog from CI, and document what was ported

Adds the release side of the update system: cliff.toml generates
CHANGELOG.md from the commit log, and scripts/add-release.py appends the
release to releases.json, which running instances fetch. The workflow does
both on tag push and commits them back.

The script is Python with its own tests rather than a few lines of inline
YAML because it has to be idempotent — re-running a failed workflow must
replace an entry rather than add a second one — and because sorting versions
as text would tell everyone running 1.10.0 that 1.9.0 is newer. Two tests in
the server assert releases.json parses and lists the version the binary
reports, so bumping Cargo.toml without publishing is a build failure rather
than a silently wrong banner.

Coolify's cliff.toml was adapted rather than copied: it keys off
conventional-commit prefixes and drops what does not match, which for this
repository's prose subjects would produce an empty changelog. The parsers
here group by what the subject says and discard nothing.

Fixes a bug found by clicking the switch in a running instance: turning the
release check off stopped future checks but left the last result on the
dashboard, so unticking the box appeared to do nothing. Both the banner and
the changelog now consult the switch on read.

Also fixes a demo bug — demo-restart recreates the target container while
keeping the database, so the registered target outlived its authorized_keys
and every deploy afterwards failed public-key authentication — and a help
target that hid every command with a digit in its name, which was all of
test-e2e.

Adds make demo-changelog to seed a pretend release so the banner and the
notes page can be seen without publishing anything.
- Harden the artifact download

artifact_url is client-supplied and the control plane fetches it, so:

- Only http and https are accepted. A file:// URL would have made the
  control plane read its own filesystem and ship the result to a target as
  a 'binary'.
- Redirects are limited to five rather than unbounded — a release asset
  legitimately redirects to object storage, but a longer chain is a loop or
  a probe.
- A declared content length over 2 GiB is refused before the body is read,
  since the artifact is buffered in memory on the way to the target.
- Enforce API tokens, and fix five dead form actions

An audit pass found two classes of real defect.

API tokens were minted, scoped, listed, revoked and sent by the CLI — but
never verified: authenticate_api_token had no caller outside its own tests,
so anything reaching the API port had full control. A tower layer now
verifies the bearer token on every non-public RPC, with read-only RPCs on
an explicit allowlist so a newly added one requires write by default.

Enforcement is opt-in via --require-api-token, because switching an
existing instance to required tokens on upgrade would lock its operator
out; when off, startup warns and names the exposure. Turning it on exposed
a genuine chicken-and-egg problem — the dashboard sends no token, so it
could not reach the API, including the page that mints tokens. The web tier
now presents one via an interceptor, and the all-in-one provisions one for
itself at boot, revoking the previous. Verified: dashboard fully usable with
enforcement on, unauthenticated CLI refused, a read token listing but
refused a mutation naming the token and RPC, a write token succeeding.

Five forms posted to paths that were never registered — service
start/stop/restart, GitHub App creation, and password change — so those
buttons 404'd. Nothing caught it: the renderer compiled, the router
compiled, and both test suites passed. Fixed, plus the missing password
route and handler, and two field-name mismatches that silently discarded
input (token scope, log line count) and a deployments filter that was
ignored. The live log and deployment panes appended a cumulative snapshot
every tick, duplicating everything; they now replace.

Added the test that catches this class: it scans render.rs for every form
action and asserts each resolves to a registered POST route. Verified it
fails when the bug is reintroduced.

686 tests.
- Detect a secret-key mismatch at startup

Running the control plane with NUDO_SECRET_KEY set and the dashboard
without it started both processes happily: the second silently generated
its own key, and the mismatch only surfaced as 'wrong key or corrupt
ciphertext' partway through opening a terminal or resolving a deploy's
secrets. Found by driving the terminal websocket end to end.

The first process to open a database now seals a verifier with its key.
Any later process holding a different key refuses to start, with a message
that names the cause and says both halves need the same key.

Also verified in the same pass, against a real systemd host: the CLI's
interactive terminal over the bidi gRPC Attach stream (login banner,
prompt, command echoed, clean logout), and the browser websocket path
(upgrade, single-use token redeemed, live PTY, resize accepted) — with the
page embedding only a session id and token and leaking no host or port.

673 tests.
- Vendor the well-known protos and verify the Docker artifact

The Docker build failed where the local build succeeded: controlplane.proto
imports google/protobuf/{timestamp,empty}.proto, and whether protoc resolves
those from its own installation depends on packaging — Homebrew ships them
on the default include path, Debian's protobuf-compiler does not. Both are
now vendored into the proto crate with that directory on the include path,
so a developer's machine, CI and the image all behave the same.

Verified: the image builds, all five binaries run, the container serves the
setup page and reports healthy. Also gave nudo-server and nudo-web the
--version flag the other three already had.
- Format the workspace and clear every clippy warning

CI denies warnings, so this makes the gates it enforces actually pass:
cargo fmt --check, clippy -D warnings across all targets, and the full
test suite. 668 unit and integration tests plus the three end-to-end
deployment tests all still pass.

One clippy suggestion was wrong and is noted where it was declined: the
parameter is a slice, so the borrow it wanted removed is required.
- Wire up the all-in-one binary and verify the dashboard end to end

The all-in-one starts the gRPC server and the dashboard in one process,
with the dashboard still reaching the server over gRPC on loopback — so
the single-box path and the split deployment exercise the same code rather
than a second wiring.

Verified against a running instance: first-run setup creates the admin and
sets a session cookie; an anonymous request to any dashboard route
redirects to /login; all eleven screens render; a form POST with a wrong
CSRF token is refused with 403; a secret stored through the UI is listed
with its digest and its value appears nowhere on the page; a
latency-critical target is badged loudly in the listing; and the audit log
attributes each action to the named human who performed it.
- Init

<!-- generated by git-cliff -->
