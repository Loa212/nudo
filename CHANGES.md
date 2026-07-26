# CHANGES

Proto changes, design decisions taken where the contract or the brief left a
choice open, and the things deliberately left out.

---

## Proto changes

**None.** `controlplane.proto` is unmodified from what was provided. Every RPC in
all eight services is implemented, including the four streams
(`WatchUnitStatus`, `Watch`, `Stream`, `RunCommand`) and the bidirectional
`Attach`.

Two places where the proto leaves a gap are handled without changing it:

- **`DeployRequest.auto_rollback_on_failure`** is documented as "default true
  server-side", but proto3 cannot distinguish an unset `bool` from `false`, so a
  literal reading makes the default impossible to honour. The server treats
  rollback as enabled unless the caller also set `skip_health_check` — a deploy
  that opts out of verification has nothing to roll back *from*. The CLI, the
  dashboard and the MCP server all send `true` explicitly, so the ambiguity only
  affects a hand-written client.

- **`Target.agent_version`** exists in the message but is always empty. The
  architecture is agentless by definition; the field is populated as `""` rather
  than removed, so a client built against the contract still compiles.

---

## Design decisions

### Deployment

**The unit's `ExecStart` points at `<release_root>/current`, never at a release
directory.** Activating a release is then a symlink swap plus a restart, and
rolling back is the identical operation aimed at an older directory. The unit
file does not change between deploys, so `systemctl cat` output is stable and a
deploy cannot half-rewrite a unit.

**The symlink swap is `ln -sfn` onto a temporary name followed by a rename.** A
plain `ln -sf` over an existing symlink is unlink-then-create, and a process
starting in that window finds nothing at all. `mv -T` is atomic; the fallback
without `-T` covers BusyBox.

**An artifact is staged outside `releases/` and moved in only after its size is
verified.** An interrupted upload therefore cannot leave a truncated binary in a
directory the symlink could be pointed at. A truncated binary that systemd then
execs is a much worse outcome than a failed deploy.

**Retention runs only after the new release is healthy.** Pruning before
verification could delete the very release a failed health check needs to roll
back to. The live release is never a candidate for pruning even when it falls
outside the retention window, and `keep_releases: 0` is treated as the default
rather than as "delete everything".

**Builds happen on the control plane, never on a target.** A latency-critical
host should not have a compiler, a package manager, or a build's memory pressure
on it. The control plane produces a binary and ships only that.

**Cancellation sets a flag the engine checks between steps** rather than killing
the task. A cancel that landed mid-swap could leave the symlink pointing at a
half-uploaded release, or a unit file half-written.

**A restart that fails pulls in `systemctl status --lines=20`** before reporting,
because a bare exit code tells an operator nothing about why a unit would not
start.

### The latency-critical guardrail

**One `authorize()` enforces it and writes the audit entry, for every mutating
RPC.** Putting the check at each call site would mean a new RPC could silently
omit it; making it one function means it cannot.

**A refused mutation is still audited.** An agent repeatedly trying to touch the
hot-path box is exactly what an operator wants to find in the log, and a refusal
that leaves no trace hides it.

**A webhook is held to the same rule as an agent.** A push to a branch that
happens to be wired to a latency-critical target does not deploy; it is refused
and audited. Automatic deployment to such a host is precisely what the flag
exists to prevent.

**`CheckTarget` is allowed against a latency-critical target without an opt-in.**
It opens one SSH connection and runs four trivial read-only commands. Refusing
would make the host you most want to verify the one you cannot.

**Creating a latency-critical target does not additionally require the
override** — declaring the target as one *is* the acknowledgement. Every later
mutation of it does require the override.

### Security

**SQLite stores no credential in a replayable form.** Session cookies, API
tokens, terminal grant tokens and GitHub setup states are kept as SHA-256
digests; secrets, App private keys and webhook secrets are sealed with
AES-256-GCM under a key the operator supplies. A copy of the database file does
not yield anyone's access.

**The terminal client sends an opaque single-use token and nothing else.** The
server looks the host, port, user and key up in its own database. Coolify's
equivalent has the browser send a full `ssh` command line and validates only the
target host, leaving `-o ProxyCommand=` — which `ssh` executes through a shell —
under client control. That is not reproduced here.

**The terminal protocol is tagged JSON in both directions.** Coolify's
server-to-client direction is raw bytes with bare sentinel strings (`pty-ready`,
`pong`, `unprocessable`), so program output equal to one of those words is
misread as a control frame. Tagging removes the ambiguity.

**Webhook signatures are verified over the raw request body, before any
parsing,** and there is no environment in which verification is skipped.
Coolify skips it entirely when its app environment is `local`. A source with no
webhook secret configured is refused rather than trusted, since its deliveries
cannot be authenticated at all.

**Every value interpolated into a remote command is shell-quoted**, and file
transfers go through `base64 -d` rather than a heredoc, so a unit file or secret
containing a quote, a newline or a delimiter is transferred exactly and cannot
inject a second command.

**Two escapers, not one, for systemd environment values.** systemd expands `$VAR`
in a unit's `Environment=` directive but not in an `EnvironmentFile`. Sharing one
escaper delivered a literal `$$` to services; they are now separate, each with a
test pinning the difference. (See "Bugs found by testing" below.)

**Secret values have no read path.** `ListSecrets` returns metadata and a SHA-256
digest; there is no RPC, no CLI flag and no page that returns a value. The
dashboard's secret rows contain no element that could hold one — following
Coolify's `is_shown_once`, which removes the input from the DOM rather than
masking it.

**The dashboard's `Secure` cookie attribute comes from the configured base URL,**
not from the request. A reverse proxy terminates TLS, so the request itself looks
plain and cannot be used to decide.

**Host keys are accepted on first use.** See "Deferred" below.

### GitHub

**Installation tokens are cached against the `expires_at` GitHub returns, with a
five-minute refresh margin.** Coolify mints a fresh token on every operation —
a JWT signature and two HTTP round-trips per clone, per branch listing, per page
of branches. The margin exists so a clone never starts with a token that expires
mid-transfer.

**Coolify's `GET /zen` clock-skew probe is not ported.** It costs an extra
round-trip on every token mint to guard against a mis-set system clock. The App
JWT already back-dates `iat` by 60 seconds and expires in 8 minutes against
GitHub's 10-minute ceiling, which absorbs ordinary skew; a clock wrong by more
than that is a host problem an operator needs to fix rather than work around.

**Deploy outcomes are written back as commit statuses,** not as pull-request
comments. Coolify posts a comment, which does not surface on the commit, in the
branch list, or in branch protection. `nudo/deploy` as a status context does all
three.

**The App manifest requests `contents: read`, `metadata: read`,
`pull_requests: read` and `statuses: write` — and not `administration`,** which
Coolify offers as an option. Nothing here administers a repository.

**`request_oauth_on_install` is false.** Nothing acts on behalf of a GitHub user,
so there is no reason to send anyone through an OAuth flow.

### Storage and API

**`sqlx::query_as` with explicit row mapping rather than the `query!` macros.**
The compile-time-checked macros need a populated database at build time, which
makes a clean checkout — and CI, and the Docker build — unbuildable without a
`cargo sqlx prepare` step and a committed `.sqlx` directory that then has to be
kept in sync. Runtime queries keep `cargo build` self-contained. Dynamic SQL is
composed only from `const` fragments with bound parameters, wrapped in
`AssertSqlSafe` with the audit noted at each site.

**Label selectors are matched in Rust, not SQL.** Labels live in a JSON column,
and the table holds tens of hosts. A schema change to make this a SQL filter
would buy nothing measurable.

**Page tokens are stringified offsets.** Opaque by convention rather than by
encryption; an unparseable token starts from the beginning instead of erroring.

**A service cannot be moved between targets.** Its releases and unit live on the
old host, so a move would silently orphan them. Delete and recreate.

**Deployments still running when the process dies are marked failed on the next
startup,** with an error saying their outcome on the target is unknown. Leaving
them "building" forever would show work in the dashboard that will never finish.

### The web tier

**The dashboard is a gRPC client, and the browser receives only HTML.** Live
views hold the upstream gRPC stream server-side, fold frames into "the latest",
and push a rendered fragment on a fixed tick with a `biased` `select!` — the
template's fold-fast/render-slow pattern. A build emitting thousands of lines a
second cannot pin the browser, and a burst cannot starve the render tick.

**Two things reach past the gRPC API into the store:** login sessions and the
webhook receiver's secret lookup. A browser session is a property of the
dashboard rather than of the deployment API, and GitHub has no cookie to
authenticate with. Everything else goes over gRPC, so the CLI, the dashboard and
the MCP server cannot drift apart.

**Front-end assets are vendored and embedded**, served from a fixed match rather
than a path lookup. No CDN fetch, no JavaScript build step, no path traversal —
and a control plane managing production hosts does not pull executable code from
a third party at page load.

**Pre-authentication forms use a fixed CSRF token.** A per-session token is
circular when there is no session yet. Those two forms create the first account
or exchange a password for a session rather than changing existing state, and
everything afterwards uses the session's own token plus a same-site cookie.

### The MCP server

**Eight coarse tools, not a mapping of every RPC.** An agent does not need
`UpdateTarget` with a field mask. Creating infrastructure, editing secrets and
holding an interactive PTY are absent entirely, with a test asserting they stay
absent — an agent should be unable to do them, not merely trusted not to.

**`deploy`, `rollback` and `run_command` default `dry_run` to true.** A mistaken
call reports a plan rather than acting. The tool descriptions say so explicitly,
and say when setting `allow_latency_critical` is and is not appropriate — those
descriptions are what determine whether an agent uses this correctly.

**Results are purpose-built shapes with enum names, not wire integers,** and a
service summary mirrors its target's `latency_critical` flag so the agent does
not have to cross-reference two calls. Log reads and command output are capped
because an agent's context is finite.

### The CLI

**`nudo exec` takes trailing var args, so nudo's own global flags must precede
the subcommand.** `nudo --allow-latency-critical exec <target> systemctl restart
bot` works; the same flag after the target is part of the remote command. This is
pinned by a test rather than left to be discovered against a production host, and
noted in `--help`.

---

## Bugs found by testing

Two bugs that only running against a real host would have caught, both now
covered by regression tests:

1. **SSH sends EOF before the exit status.** The receive loop ended on
   `ChannelMsg::Eof`, so every remote command reported failure however well it
   ran. The symptom was the readiness probe printing `systemd 252 (252.39-1)` and
   marking the systemd check FAILED. Every `systemctl` call in the product was
   affected. Only `Close` now ends the loop.

2. **`$` was escaped as `$$` in `EnvironmentFile` values.** Correct for a unit's
   `Environment=` directive, wrong for an `EnvironmentFile`, so a secret
   containing `$` reached the service with a literal `$$`. Split into two
   escapers.

3. **A secret-key mismatch between the two processes failed silently until it
   mattered.** Running the control plane with `NUDO_SECRET_KEY` set and the
   dashboard without it started both happily — the second generated its own key
   — and the mismatch surfaced as "wrong key or corrupt ciphertext" partway
   through opening a terminal. The first process to open a database now seals a
   verifier, and any later process holding a different key fails at startup with
   a message naming the cause and the fix. Found by driving the terminal
   websocket end to end.

Three more were caught by unit tests before any real run: the `rm -rf` guard on
service deletion sat *after* the SSH connect and so was unreachable; `Rollback`'s
response did not carry the release it targeted; and MCP requires an object at the
root of a tool's output schema, so the listing tools' bare arrays were a spec
violation.

---

## Deferred

Everything below is deliberately out of scope, not stubbed. There are no
`todo!()`, `unimplemented!()`, mocked internals or placeholder handlers in the
delivered code.

**SSH host-key pinning.** Host keys are accepted on first use. Recording a
target's key on first connect and refusing a change would defend against a
man-in-the-middle on a re-registered host. It needs a UI for reviewing and
accepting a legitimate change (a rebuilt host has a new key), which is a feature
in itself rather than a line of code. The exposure is bounded: an operator
registers targets explicitly, by address.

**Pull-request preview environments.** `pull_request` deliveries are
authenticated, acknowledged with 200 so GitHub does not retry and disable the
hook, and recorded — but they do not deploy. A preview environment needs an
addressing scheme, DNS or a proxy, and a lifecycle for tearing it down; on
bare-metal systemd targets it also needs port allocation. Coolify's
implementation of this is substantial and assumes Docker for the isolation.

**Fork-PR author-association gating.** Follows from the above; there is nothing
to gate.

**Multi-user roles and teams.** Every account is an administrator. The brief
specifies session auth plus scoped API tokens, which is what is implemented;
role separation across many operators is a different product shape.

**Requiring API tokens is opt-in, not the default.** Tokens are minted, scoped
(`read`/`write`), listed, revoked, audited, and — with `--require-api-token` —
verified on every gRPC call by a tower layer, with read RPCs on an explicit
allowlist so a newly added RPC needs `write` by default. It is off unless asked
for, because the intended deployment binds the API to loopback with the dashboard
in front of it, and switching an existing instance to required tokens on upgrade
would lock its operator out. When it is off the server logs a warning at startup
naming the exposure.

The dashboard needs a credential of its own once enforcement is on, or it cannot
reach the API — including the page that mints tokens, which would be exactly that
lockout. The all-in-one provisions one for itself at boot and revokes the previous
one; a split deployment sets `NUDO_TOKEN` on the web tier.

**GitHub Enterprise Server end-to-end verification.** URL derivation for
github.com, Enterprise Cloud (`*.ghe.com`) and Enterprise Server is implemented
and unit-tested, and the manifest and install paths differ correctly per host
family. Only github.com has been exercised against a mock; no Enterprise
instance was available.

**Deploy-key registration on GitHub's side.** The deploy-key clone path is
complete — the key is sealed, written to a `0600` temporary file for the clone's
lifetime, and used through `GIT_SSH_COMMAND`. Registering the public half with
GitHub via `POST /repos/{owner}/{repo}/keys` is not automated; the operator pastes
it, exactly as in Coolify.

**Metrics and alerting.** No Prometheus endpoint, no notification channels. The
audit log and deployment history cover "what happened"; "tell me when it
happens" is a separate concern.

**arm64 release artifacts.** CI builds `x86_64-unknown-linux-gnu` and
`x86_64-unknown-linux-musl`, as specified. Adding aarch64 is a matrix entry and a
cross-linker.
