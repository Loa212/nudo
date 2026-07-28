# CHANGES

Proto changes, design decisions taken where the contract or the brief left a
choice open, and the things deliberately left out.

---

## Refactor behavior changes

The repository-wide module refactor deliberately includes four operator-visible
fixes. They are called out here because they are not mechanical file moves:

- **Masked target and service updates are atomic.** All SQL statements for one
  update mask now share a transaction. A later failure rolls back earlier field
  writes instead of returning an error after partially changing the row. This
  holds SQLite's write lock for the duration of the masked update rather than
  for each individual statement.
- **A forbidden service target move is validated before any write.** A request
  that includes both `target_id` and another field now persists neither field.
  Previously the other field could autocommit before the move was refused.
- **Duplicate service renames return the same readable conflict as creation.**
  The raw SQLite `UNIQUE constraint failed` text is now translated to
  `a service named ... already exists on that target`.
- **Temporary artifact and MCP session tokens use UUIDs.** The previous
  timestamp-and-process-id values were guessable from creation time; UUIDs keep
  the existing URL and audit prefixes while making the token unpredictable.

---

## Proto changes

**One addition: build hosts.** Every RPC in the original eight services is
implemented and unchanged, including the four streams (`WatchUnitStatus`,
`Watch`, `Stream`, `RunCommand`) and the bidirectional `Attach`. A ninth
service, `BuildHosts`, was added along with:

- `BuildHost`, and its request/response messages, mirroring `Target`'s shape.
- `GitSource.build_host_id` (field 7), a new optional field. Unset — which is
  what every existing client sends — means the instance default, which is itself
  the control plane until an operator changes it. Nothing that predates this
  builds anywhere new.
- `BuildDefaults`, and `GetDefaults`/`SetDefaults` on the new service.

One further field: `PutSecretRequest.replace` (field 6), which turns what used
to be an unconditional upsert into a refusal unless the caller asks to replace.
Unset is the safe default, so an existing client cannot overwrite a secret by
accident — see *Secrets* below for why that mattered enough to change.

`CheckBuildHostResponse` carries a `warnings` list alongside `checks`, which
`CheckTargetResponse` has no equivalent of. A warning must not make `ok` false:
a latency-critical build host is a choice an operator made, and folding it into
the checks would fail a CI step gating on readiness for a host that is working
exactly as configured.

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

**Builds never happen on a target.** A latency-critical host should not have a
compiler, a package manager, or a build's memory pressure on it. They run on the
control plane by default, or on a build host — see *Build hosts* below — and a
binary is all that reaches the target either way.

**Cancellation sets a flag the engine checks between steps** rather than killing
the task. A cancel that landed mid-swap could leave the symlink pointing at a
half-uploaded release, or a unit file half-written.

**A restart that fails pulls in `systemctl status --lines=20`** before reporting,
because a bare exit code tells an operator nothing about why a unit would not
start.

### Build hosts

The issue that asked for these left three questions open. All three were
decided deliberately; the reasoning is here because the code cannot explain a
road not taken.

**A build host is its own entity, not a `targets` row with a flag.** They share
reachability, an SSH user and a key, and nothing else: a build host has no
release root, no unit, no `latency_critical` semantics in the deploy sense, and
nothing is ever deployed to it. Reusing `targets` would have been less code and
would have produced a "target" that is never deployed to, carrying columns that
mean nothing for it — and it would have made "a build host is not a deploy
target" a rule enforced by remembering rather than by the schema. The two nouns
are separate everywhere: table, proto service, CLI noun, dashboard page.

**The checkout is cloned on the build host, not uploaded to it.** The
alternative keeps credentials on the control plane at the cost of transferring a
tree over the existing base64 SSH upload path, which is slow for a large
repository. Cloning there is cheaper and keeps one code path for credential
handling, and the cost is stated rather than hidden: the build host receives
repository credentials. A deploy key is written to a `0600` file for the clone's
lifetime and removed with the workspace; an App token rides the command line as
it does locally and is redacted from output before it reaches a log. This is
why a build host's SSH host key is pinned and verified on exactly the same terms
as a target's — connecting to the wrong machine here hands it a key.

**A latency-critical build host is allowed, and warned about.** Refusing it
outright was the tidier rule and the wrong one: an operator may have exactly one
spare machine, and nudo refusing to use it does not make the build go away — it
makes the build stay on the control plane, which may be worse. So it is
permitted, mutating it needs `allow_latency_critical` like any other
latency-critical host, and the contention is stated everywhere it is relevant:
`check` returns it as a warning, the dashboard renders a callout, the CLI prints
it after the checks. What is *not* done is failing the check, which would gate
CI on a host that is working exactly as configured.

**The deploy log does not reveal where a build ran.** Same lines, same order,
same redaction, local or remote. Where a build happens is configuration, not
output, and anything parsing a deployment log should not break when an operator
adds a build host.

**The workspace is removed on every exit path** — success, failure, timeout, a
lost connection. A build host that accumulates checkouts fills up, and that
failure then belongs to every service that builds there. Cleanup is best-effort
and never fails a build that produced a binary; the `rm -rf` is guarded against
ever being aimed at a path shallower than two directories deep.

**Deleting a build host leaves its services pointing at it.** They fail their
next build with a message naming the missing id, rather than silently falling
back to the default — which would move a build nobody asked to move. The
instance default *is* cleared on deletion, because a dangling default would fail
every git-backed deploy on the instance at once. The deletion's audit entry
names the services left behind.

**`local` is a sentinel `build_host_id`, distinct from empty.** Empty means
"whatever the instance default is"; `local` means the control plane whatever the
default is. Without the distinction, pointing an instance at a build host would
silently move every service that was deliberately building locally.

**Isolation between builds is out of scope, and said so in the docs.** Running
two mutually distrusting build commands on one host is a real problem, and it is
a property of how that host is run — one-shot container, ephemeral VM, fresh
instance per build — not something nudo can implement inside itself. The risk is
that registering a build host *reads* as a sandbox, so the dashboard says
otherwise on every build host's page, not only shared ones.

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

**SSH host keys are pinned on first use and verified on every connection after
that.** The first successful connection to a target records the key it presented;
every later one compares against it and fails closed on a mismatch, with an error
naming both fingerprints. This matters because nudo holds the private key for
every target it manages: without verification, a man-in-the-middle on a
re-addressed or re-registered host is handed an authentication attempt with that
key, and nothing in the protocol notices. The check happens in
`check_server_key`, during the handshake — so a host that is not the one we
pinned is refused *before* authentication, which is the only ordering that
actually protects the key.

**A changed host key blocks read-only operations too.** `targets check` and
`logs` are otherwise allowed against latency-critical hosts on the principle that
the host you most want to inspect should not be the one you cannot. A host-key
mismatch is a different situation: it may not be that host at all, and reading
logs from the wrong machine is its own problem. So everything is refused —
deploys, logs, terminals, checks — until the change is reviewed.

**A changed key is held for review rather than only reported.** The presented key
is recorded alongside the pinned one, so the dashboard can show what changed and
an operator can accept it in one click; the CLI equivalent is `nudo targets
host-key <id> --accept <fingerprint>`. Accepting names the fingerprint being
accepted, which must match what is pending — otherwise a key that changed again
between the review and the click would be accepted unseen. Acceptance is audited
with the fingerprint and the actor, and is subject to the latency-critical
guardrail like any other mutation.

**Trust on first use rather than confirmation at registration.** Showing the
fingerprint when a target is created and asking for confirmation would be
stronger, but it adds a step to a flow that is currently one form, and an
operator who has not yet reached the machine has nothing to compare against.
First use is what most tools do and is the weaker half of the guarantee: it
cannot tell you the first key was the right one, but it will not let the key
change behind your back. `nudo targets host-key <id> --forget` reopens the
window for a host rebuilt while nobody was watching.

**Existing targets pin on their next connection rather than being refused.** The
migration leaves the column empty, which is exactly the state a new target is in,
so upgrading a working fleet does not break it.

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

### Secrets

**Writing a name that already exists is refused, not an overwrite.** The store
upserted, so `Secrets.Put` on a taken name silently replaced the value. Every
other resource here can be inspected after a mistaken write; a secret cannot —
it is write-only by design, so an accidental overwrite destroys something
unrecoverable and leaves nothing to say what was lost. The name is the same and
only the digest moved, which is only noticeable to someone who wrote the old
digest down.

Rotation is a real operation — a leaked key has to be replaceable — so it stays
available behind `PutSecretRequest.replace`. Unset is the safe default and what
an existing client sends, so nothing that predates this can overwrite by
accident. The dashboard exposes it as a **Rotate** action on the secret's row
rather than as a flag on the add form: opening it says what the replacement
costs before there is anything to submit, and the ordinary write can never reach
it. The CLI spells it as its own verb, `nudo secrets rotate`, rather than a flag
on `set`: a flag left behind in a shell-history line would quietly restore the
behaviour this changed, and a separate command cannot be reached by accident.

A rotation is audited as `rotated secret X` rather than `stored secret X`.
Storing something new and destroying something unrecoverable are different
events, and the second is the one worth finding later.

**The refusal is reported on the page rather than as an error page.** Somebody
typing a name that is taken is an expected outcome, not a failure of the system,
and the message belongs where they typed it. It survives the redirect that
follows the POST as a key in the query string, resolved to text server-side — a
crafted link cannot put arbitrary words in a banner on an operator's own
dashboard.

**An SSH key is asked for in the shape an SSH key has.** The same store either
way, but a key is multi-line, is never an environment variable, and has no
meaningful target or service scope — so it gets a textarea, no env-var framing,
and no scope fields. It is also the one value here that can be checked before
storing: a public key or a truncated paste is otherwise accepted, encrypted, and
surfaces much later as a failed connection to a host nobody has reason to doubt.
The check is a shape check rather than a parse, since nudo accepts several key
formats and refusing an unrecognised one would be worse than storing it.

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

### What was taken from Coolify's update system, and what was not

The release check, the in-app changelog and the "support this project" prompt are
ported from Coolify. Three things were kept and three deliberately were not.

**Kept: the anti-rollback ladder.** Coolify compares the version it fetched, the
version it last recorded, and the version actually running, and takes the newest
of the three. Without that, a manifest that momentarily serves an older entry
tells everyone to downgrade. `best_known_version` does the same.

**Kept: monthly recurrence on the support prompt.** The button says "Maybe next
time", not "Never", and the prompt returns the following calendar month. A
permanent dismissal on first sight means it is never seen again by the people
most likely to have come to depend on the tool later.

**Kept: one instance-wide off switch.** Someone who genuinely does not want to be
asked should be able to say so once, and have it stick.

**Not kept: the self-updater.** Coolify's update path downloads a shell script
from a CDN and executes it as root on the host. For a tool that holds every
target's SSH keys, that is a large amount of trust to place in a URL.

What replaced it is an instructions page rather than a button. `/upgrade` detects
how the instance is installed — `/.dockerenv`, the image's own environment
marker, or `/proc/1/cgroup` — and prints the exact commands for that case only: a
`docker pull` and recreate for a container, a download-verify-replace-restart
sequence for a binary under systemd. Showing a container operator how to restart
a systemd unit would be worse than showing nothing, so each kind sees only its
own, and tests assert the other's instructions are absent.

The page leads with what upgrading does to your data, because that is the first
question anyone has: it replaces executables and nothing else, since the
database, the data directory and the configuration all live outside them, and
migrations run automatically when the new version opens the database. Verified by
building an instance with an account and an encrypted secret, then doing the full
`docker stop` / `rm` / `run` cycle and confirming the session, the account and the
secret all survived.

Two tests keep this a page rather than a button: one asserts the banner submits
nothing and contains no shell command, the other that the upgrade page has no
`<form>` and pipes nothing into a shell.

A self-updating binary — fetch the release, verify the checksum and a signature,
then use the same atomic swap and rollback nudo already performs for services,
pointed at itself — is deferred rather than rejected. It can only work for the
binary install, since a process cannot replace its own container image.

**Not kept: the telemetry.** A Coolify instance pings `undead.coolify.io` on
every boot, on by default. nudo sends nothing, ever. There is no setting to
disable it because there is nothing to disable, and a test scans the support
module for a network client so that stays true.

**Not kept: localStorage dismissal.** Coolify stores the sponsorship dismissal in
the browser, so it resets on every new browser and cannot be honoured across
devices. nudo stores it against the user.

**Added: a gate before anyone is asked.** Coolify's popup can appear on a
completely empty instance. nudo's waits until five deployments have succeeded.
Asking for support before someone has deployed anything is asking a stranger for
money.

**Adapted rather than copied: the changelog generator.** Coolify's `cliff.toml`
keys off conventional-commit prefixes and drops anything that does not match.
This repository writes commit subjects as sentences, so that configuration would
put every commit in "Other" and filtering would produce an empty changelog. The
parsers here group by what the subject says and discard nothing: a generated
changelog that silently omits a commit is worse than a noisy one, because the
omission is invisible to someone reading it to decide whether to upgrade.

---

## Bugs found by testing

Bugs that only running the thing would have caught, all now covered by regression
tests:

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

4. **Five rendered form actions pointed at routes that did not exist.** Service
   start/stop/restart, GitHub App creation and password change all posted to
   paths the router never registered, so those buttons 404'd. Nothing caught it:
   the renderer compiled, the router compiled, and both test suites passed. There
   is now a test that scans the renderer for every form action and asserts each
   resolves to a registered POST route — verified to fail when the bug is
   reintroduced. Two field-name mismatches in the same class (token scope, log
   line count) were silently discarding input.

5. **Turning the release check off froze the banner instead of clearing it.**
   The switch stopped future checks, but the dashboard kept rendering whatever
   the last check had found — so unticking the box appeared to do nothing, which
   is the worst possible outcome for a switch whose whole purpose is to stop
   something being shown. Found by clicking it in a running instance. Both the
   banner and the changelog now consult the switch on read; the recorded row is
   left alone, so turning it back on restores what was known without waiting for
   the next check.

Three more were caught by unit tests before any real run: the `rm -rf` guard on
service deletion sat *after* the SSH connect and so was unreachable; `Rollback`'s
response did not carry the release it targeted; and MCP requires an object at the
root of a tool's output schema, so the listing tools' bare arrays were a spec
violation.

6. **The login and setup pages rendered the navigation rail.** Both build their
   own document via `auth_shell` — and `login_page`'s doc comment says so —
   but all four call sites wrapped them in `page()` as well, which adds the
   rail. So a signed-out visitor got the full navigation beside the login form,
   every item of which redirects straight back to login. Two nested documents,
   two `<!DOCTYPE>`s. Found by looking at the page. There is now a test that
   asserts the login and setup pages carry no rail and exactly one document,
   verified to fail against the old code.

7. **The "is the musl build static?" check failed every correct build.** It
   grepped `ldd` output for "not a dynamic executable" with `grep -qv`, which
   matches *any* other line — and a static musl binary makes `ldd` print
   "statically linked" instead, so the check rejected exactly the binaries it
   was meant to pass. It only surfaced on the first real release, because
   nothing tags on an ordinary push. Now it reads the ELF dynamic section for
   `NEEDED` entries, which is the actual property: depends on no shared library
   at run time. Deliberately not INTERP — Rust links musl targets as
   `-static-pie`, so a genuinely static binary still has an interpreter segment,
   and testing for that would reject every binary the job produces. Verified
   against both a static musl build and a dynamic gnu one.

Three more surfaced only when the first real release ran, because nothing tags
on an ordinary push: `actions/download-artifact` with no pattern also tried to
fetch the `.dockerbuild` build record that `docker/build-push-action` uploads on
its own, failed on it, and took the publish step down *after* both real archives
had already downloaded and verified; the release body told people to
`docker pull ghcr.io/Loa212/nudo:v0.1.0`, which is wrong twice over, since GHCR
paths are lowercase and `docker/metadata-action`'s semver pattern strips the
leading `v` (the published tags are `0.1.0`, `0.1` and `latest`); and the
manifest's release notes were the whole GitHub release body, so the dashboard's
"What's new" showed `tar -xzf` and `docker run` install instructions to people
who are, by definition, already running it. The notes now come from the
generated `CHANGELOG.md` section for that version, via a script with its own
tests.

A fourth surfaced the same way: `git-cliff` was invoked without `--tag`, and
because the publish job checks out `main` — where the tag is not the tip, since
a previous run's manifest commit sits above it — every commit was filed under
"Unreleased". `CHANGELOG.md` never grew a `## 0.1.0` heading, so the notes
extracted for the manifest fell back to "see the release page". The fallback
behaved correctly; it was just covering for a missing argument.

8. **First-run setup could not be completed from the browser.** The handler
   required a `display_name` field the form has never rendered, so every real
   signup failed with `Failed to deserialize form body: missing field
   display_name`. Nothing caught it because every test — and every demo script —
   built the request by hand and happened to include the field, so the one path
   nobody exercised was the one every new user takes first. The form now decides:
   `display_name` is optional and defaults to the email's local part.

   The same handler also discarded `password_confirm`, which the form has always
   rendered and asked people to type. A mismatch was accepted silently, creating
   an account with a password nobody knew — on an instance where setup closes
   itself immediately afterwards, so there is no second attempt. Both are now
   covered by a test that submits exactly the fields the rendered page contains,
   rather than a request an author remembered to assemble.

9. **The confirmation check broke every non-browser caller.** Requiring
   `password_confirm` to equal `password` also rejected callers that sent no
   confirmation at all — including this repository's own demo scripts, so
   `make demo` stopped being able to create its account. An absent confirmation
   is not a mismatch: the form always renders the field, so a disagreeing pair
   is a real typo, but nothing to compare against is not. Found by running
   `make demo` after the fix rather than only the test suite.

10. **Host-key pinning would have refused every host whose key carried a
    comment.** `ssh-key`'s `PartialEq` for a public key includes the comment,
    and both `ssh-keygen` and `ssh-keyscan` emit one — so a pinned key and the
    identical key presented by the host compared unequal, with matching
    fingerprints, and read as *changed*. That is the worst shape a security
    check can fail in: it would have blocked a working fleet while reporting a
    compromise that had not happened, and the fingerprints printed side by side
    in the error would have been the same string. Comparison is now on the key
    material alone, and keys are stored re-encoded without the comment so what
    is pinned, compared and displayed is one canonical form. Caught by a unit
    test written to pin the round trip through a text field.

11. **A pinned key that would not parse silently re-pinned whatever answered.**
    The verdict matched on `parse_host_key` returning `None`, which conflates
    "nothing pinned yet" with "the pinned value is corrupt" — so a damaged
    column downgraded that target from verification to trust-on-first-use, with
    no error and nothing in the UI to show it had happened. An unparseable
    pinned value is now a mismatch, not an absence: it fails closed and
    surfaces as a change to review. Found while writing the test for the empty
    case and asking what the other way into that branch was.

Two were in the demo rather than the product: `service_id_by_name` shelled out
to the CLI purely to look up an id, so `make demo-examples` failed on a fresh
checkout — before reaching the one step that genuinely needs a compiled binary.
It now reads the services page over HTTP, like `target_id` beside it. And the `make demo-restart` recreates the
target container while keeping nudo's database, so the registered target outlived
the `authorized_keys` it depended on and every subsequent deploy failed public-key
authentication. The setup script now reinstalls the key even when the target is
already registered, and `demo-restart` runs it.

---

## Deferred

Everything below is deliberately out of scope, not stubbed. There are no
`todo!()`, `unimplemented!()`, mocked internals or placeholder handlers in the
delivered code.

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

**A dedicated `Sources` RPC for creating a deploy-key source.** The clone path is
complete and the store seals the key, but the proto's `Sources` service has only
the manifest RPCs plus List and Delete — so a deploy-key source has no creation
surface short of writing the row directly. Adding one means extending the proto,
which is permitted but was not worth doing without a matching UI and CLI verb.

**`nudo services create` / `update`.** Services are created in the dashboard or
over the API; the CLI reads and acts on them but does not define them. A pure-CI
workflow that provisions a service from scratch therefore needs the API directly.

**Commit statuses for deploys not triggered by a push.** A webhook deploy reports
its outcome back to the commit; one started from the dashboard or the CLI against
a git-backed service does not, even though the SHA is known.

**Metrics and alerting.** No Prometheus endpoint, no notification channels. The
audit log and deployment history cover "what happened"; "tell me when it
happens" is a separate concern.

**A self-updating binary.** `/upgrade` prints the commands; it does not run them.
Doing it properly means fetching the release, verifying both the checksum and a
signature, and then performing the same staged-swap-and-rollback nudo already
does for the services it deploys — pointed at itself, which is harder, because
the thing performing the upgrade is the thing being replaced. It also only
applies to the binary install: a process cannot swap its own container image, so
the containerised path stays `docker pull` regardless. Deferred as a deliberate
scope call rather than a rejection; the detection and the instructions it would
build on are already in place.

**arm64 release artifacts.** CI builds `x86_64-unknown-linux-gnu` and
`x86_64-unknown-linux-musl`, as specified. Adding aarch64 is a matrix entry and a
cross-linker.
