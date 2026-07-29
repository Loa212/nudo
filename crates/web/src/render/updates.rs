use super::*;

// ---------------------------------------------------------------------------
// Updates and the changelog
//
// The banner and the "What's new" page. Both render data the control plane has
// already fetched — this module never makes a request of its own.
// ---------------------------------------------------------------------------

/// The banner shown when a newer release exists.
///
/// Renders nothing when the instance is current or the operator skipped this
/// release, so the caller can place it unconditionally.
///
/// The banner itself still submits nothing and contains no shell command —
/// what it offers is a look at what changed. On an install that can upgrade
/// itself, the dialog it opens carries the action; on every other install the
/// same dialog sends you to the page with the commands for your install kind.
pub fn update_banner(status: &UpdateBanner) -> Markup {
    if !status.available {
        return html! {};
    }

    html! {
        div class={ "callout " @if status.breaking { "bad" } @else { "info" } } .update-banner {
            strong {
                "nudo " (status.latest) " is out"
                @if status.breaking { " — it needs manual steps" }
            }
            p {
                "You are running " (status.current) ". "
                @if status.breaking {
                    "Read the notes before upgrading: this release changes something \
                     that will not migrate itself."
                } @else {
                    "See what changed, then decide."
                }
            }
            div .form-actions {
                // Opens the dialog rendered by `update_dialog`. A plain link
                // to the fragment so it still works without JavaScript: the
                // dialog is `:target`-driven, not script-driven.
                a .btn.small.primary href="#whats-new" { "What's new" }
                a .btn.small href="/upgrade" { "How to upgrade" }
                @if !status.url.is_empty() {
                    a .btn.small href=(status.url) target="_blank" rel="noreferrer noopener" {
                        "Release notes"
                    }
                }
            }
        }
    }
}

/// The update dialog: what changed, and what to do about it.
///
/// Opened from the banner. Shown as a modal via `:target`, so it needs no
/// JavaScript and survives the page reloads that the actions cause — which
/// matters most while an upgrade is running, since the process serving this
/// page is the one being replaced.
///
/// Three ways out, and they mean different things. **Update now** appears only
/// on an install that can actually do it, and starts the verified staged
/// upgrade. **Skip this version** records the decision so the banner stops
/// asking about this release, and stays quiet until a newer one lands. **Close**
/// decides nothing, and the banner is there next time.
pub fn update_dialog(view: &UpdateDialog) -> Markup {
    if !view.available {
        return html! {};
    }

    let in_flight = view
        .self_upgrade
        .as_ref()
        .is_some_and(|status| status.in_flight());

    html! {
        // The backdrop is a link back to "no fragment", so clicking outside
        // the dialog closes it the way a modal should.
        div #whats-new .modal {
            a .modal-backdrop href="#" aria-label="Close" {}
            div .modal-card role="dialog" aria-modal="true" aria-labelledby="whats-new-title" {
                div .modal-head {
                    div {
                        h2 #whats-new-title { "nudo " (view.latest) " is available" }
                        p .small.muted { "You are running " (view.current) "." }
                    }
                    a .modal-close href="#" aria-label="Close" { "×" }
                }

                div .modal-body {
                    @if view.breaking {
                        (callout("bad", "This release needs manual steps", html! {
                            p {
                                "Something in this release does not migrate itself. \
                                 Read the notes below before starting."
                            }
                        }))
                    }

                    @if in_flight {
                        // The upgrade is running: the notes give way to
                        // progress, polled so the restart is ridden through.
                        (upgrade_progress(view.self_upgrade.as_ref().expect("in flight")))
                    } @else {
                        @if view.notes.is_empty() {
                            p .muted { "No notes were published for this release." }
                        } @else {
                            div .release-notes { (release_notes(&view.notes)) }
                        }
                        @if !view.url.is_empty() {
                            p .small {
                                a href=(view.url) target="_blank" rel="noreferrer noopener" {
                                    "Full notes for " (view.latest)
                                }
                            }
                        }
                    }
                }

                @if !in_flight {
                    div .modal-foot {
                        // "Close" first in the DOM but visually last: the
                        // least consequential action should not be the one
                        // the eye lands on, and should not be the default
                        // submit either.
                        div .modal-foot-actions {
                            a .btn href="#" { "Close" }
                            form method="post" action="/upgrade/skip" {
                                (csrf_input(&view.csrf))
                                input type="hidden" name="version" value=(view.latest);
                                button .btn type="submit" { "Skip this version" }
                            }
                            @match &view.self_upgrade {
                                Some(status) if status.can_upgrade_now() => {
                                    form method="post" action="/upgrade/start" {
                                        (csrf_input(&view.csrf))
                                        input type="hidden" name="target_version" value=(view.latest);
                                        button .btn.primary type="submit" { "Update now" }
                                    }
                                }
                                _ => {
                                    a .btn.primary href="/upgrade" { "How to update" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The in-progress body of the dialog, polled while an upgrade runs.
///
/// Polling rather than a stream on purpose: the connection dies when the
/// process execs itself, and a poll that quietly fails and retries is what
/// carries the dialog across the restart. `hx-target` is the region itself, so
/// each reply replaces only this.
fn upgrade_progress(status: &SelfUpgradeView) -> Markup {
    html! {
        div #upgrade-progress
            hx-get="/upgrade/status"
            hx-trigger="every 2s"
            hx-swap="innerHTML" {
            (self_upgrade_status_fragment(status))
        }
    }
}

/// The upgrade instructions, for the way this instance is actually installed.
///
/// Mostly a page, with one deliberate exception. A container install and a
/// legacy binary install get exact commands to run on the host. A managed
/// binary install — one running from the self-release layout, with both the
/// config flag and the settings toggle opted in — gets a button, because for
/// that install nudo can do the whole staged, digest-verified, rolled-back
/// sequence itself. What it will never get is a shell: the upgrade downloads
/// an artifact, verifies it against the digest committed to the repository,
/// and execs it. No script is fetched, and nothing on this page pipes into a
/// shell — the tool this was modelled on curls a script from a CDN and runs
/// it as root, and the distance from that is the point.
///
/// The commands are exact and the reasoning is stated, because the questions
/// someone actually has at this point are "will this lose my data" and "what if
/// it goes wrong".
pub fn upgrade_page(view: &UpgradeView) -> Markup {
    html! {
        (topbar(
            "Upgrading nudo",
            Some(&format!("running {}", view.current)),
            html! { a .btn href="/changelog" { "What's new" } },
        ))
        div .content {
            @if view.available {
                (callout("info", &format!("nudo {} is available", view.latest), html! {
                    p { "You are running " (view.current) "." }
                }))
            } @else {
                (callout("info", "You are up to date", html! {
                    p {
                        "Nothing to do — these are the steps for when there is. "
                        "The version here is " (view.current) "."
                    }
                }))
            }

            @if view.breaking {
                (callout("bad", "This release needs manual steps", html! {
                    p {
                        "Read the release notes before starting. Something in this \
                         release does not migrate itself."
                    }
                }))
            }

            div .card {
                h2 { "Your data is not touched by any of this" }
                div .card-body {
                    p {
                        "Upgrading replaces executables. Everything nudo remembers \
                         lives outside them and is left exactly as it is:"
                    }
                    ul {
                        li { "the database — targets, services, deployment history, sessions" }
                        li { "the data directory — build workspaces and uploaded artifacts" }
                        li { "your configuration — environment variables or the systemd unit" }
                    }
                    p {
                        "Schema changes are applied automatically the first time the \
                         new version opens the database, so there is no migration \
                         step to run by hand."
                    }
                    (callout("warn", "The one thing worth checking first", html! {
                        p {
                            "If you never set a secret key, nudo generated one into the \
                             data directory and warned you at startup. It is still there \
                             and still works — but every stored secret is unreadable \
                             without it, so back it up before doing anything that could \
                             remove that directory."
                        }
                    }))
                }
            }

            // The tag to pull: the new version when there is one, otherwise
            // `latest` — pulling the version already running is a no-op, and
            // printing it as an instruction is just confusing.
            @let tag = if view.available { view.latest.as_str() } else { "latest" };

            @match &view.install {
                UpgradeInstall::Container { image } => (container_upgrade(image, tag)),
                UpgradeInstall::BinaryLegacy => (binary_upgrade(tag)),
                UpgradeInstall::BinaryManaged { status } => (managed_upgrade(view, status)),
            }

            div .card {
                h2 { "If it goes wrong" }
                div .card-body {
                    p {
                        "Run the previous version again — it is the same command with \
                         the older tag, or the binaries you moved aside. The database \
                         is compatible in the direction you have already come from, so \
                         going back works as long as the older version has seen that \
                         schema before."
                    }
                    p .small.muted {
                        "Downgrading across a release marked as needing manual steps is \
                         the exception: check its notes, which say what changed."
                    }
                }
            }
        }
    }
}

/// Upgrade steps for a containerised install.
fn container_upgrade(image: &str, latest: &str) -> Markup {
    let pull = format!("docker pull {image}:{latest}");
    html! {
        div .card {
            h2 { "This instance is running in a container" }
            div .card-body {
                p {
                    "Upgrading means pulling the new image and recreating the \
                     container. The state volume is not part of the image, so \
                     recreating the container keeps everything."
                }
                pre .code {
                    (pull) "\n"
                    "docker stop nudo\n"
                    "docker rm nudo\n"
                    "# then run it again with your usual flags, using the new tag"
                }
                p .small.muted {
                    "Using compose instead: " code { "docker compose pull" } " then "
                    code { "docker compose up -d" } " — which does the same thing and \
                     keeps your flags where you already wrote them down."
                }
                (callout("warn", "Check for the volume before you remove anything", html! {
                    p {
                        "Recreating the container is only safe because the database \
                         lives on a volume. If you started nudo without one, its state \
                         is inside the container and removing it destroys that state. "
                        code { "docker inspect -f '{{ .Mounts }}' nudo" }
                        " says which you have."
                    }
                }))
            }
        }
    }
}

/// Upgrade steps for a binary install.
///
/// `version` is the release to fetch, or `latest` when the instance is already
/// current — in which case the snippet is an illustration rather than something
/// to paste, and says so.
fn binary_upgrade(version: &str) -> Markup {
    let is_placeholder = version == "latest";
    html! {
        div .card {
            h2 { "This instance is running as a binary on the host" }
            div .card-body {
                p {
                    "Download the release archive, verify it, and replace the \
                     binaries. Nothing under the data directory is touched."
                }
                pre .code {
                    @if is_placeholder {
                        "version=X.Y.Z   # the release you are upgrading to\n"
                    } @else {
                        "version=" (version) "\n"
                    }
                    r#"target=x86_64-unknown-linux-musl   # or -gnu"# "\n"
                    r#"base=https://github.com/loa212/nudo/releases/download/v$version"# "\n"
                    "\n"
                    r#"curl -fLO "$base/nudo-v$version-$target.tar.gz""# "\n"
                    r#"curl -fLO "$base/nudo-v$version-$target.tar.gz.sha256""# "\n"
                    r#"sha256sum -c "nudo-v$version-$target.tar.gz.sha256""# "\n"
                    "\n"
                    r#"tar -xzf "nudo-v$version-$target.tar.gz""# "\n"
                    r#"sudo systemctl stop nudo"# "\n"
                    r#"sudo install "nudo-v$version-$target"/nudo* /usr/local/bin/"# "\n"
                    r#"sudo systemctl start nudo"# "\n"
                }
                p {
                    "The checksum step is not optional decoration: it is what \
                     distinguishes the release you meant to install from whatever \
                     the network handed you."
                }
                p .small.muted {
                    "Keep the old binaries until the new version has started and you \
                     have loaded a page — " code { "sudo cp /usr/local/bin/nudo-all-in-one /tmp/nudo.previous" }
                    " before installing makes going back a single command."
                }
                (callout("info", "Optional: let nudo do this for you next time", html! {
                    p {
                        "An install that runs from a versioned release directory can \
                         upgrade itself from this page — staged, verified against the \
                         digest in the release manifest, and rolled back automatically \
                         if the new version cannot start. Adopting the layout is a \
                         one-time move:"
                    }
                    pre .code {
                        "version=" (if is_placeholder { "X.Y.Z   # the version you are running" } else { version }) "\n"
                        r#"sudo install -d -o nudo -g nudo /var/lib/nudo/self/releases/$version"# "\n"
                        r#"sudo install -o nudo -g nudo nudo-v$version-*/nudo* /var/lib/nudo/self/releases/$version/"# "\n"
                        r#"sudo install -o nudo -g nudo nudo-v$version-*/nudo-boot-guard /var/lib/nudo/self/nudo-boot-guard"# "\n"
                        r#"sudo ln -sfn releases/$version /var/lib/nudo/self/current"# "\n"
                        r#"sudo chown -h nudo:nudo /var/lib/nudo/self/current"# "\n"
                    }
                    p {
                        "Then point the unit at the layout — "
                        code { "ExecStart=/var/lib/nudo/self/current/nudo-all-in-one" } ", "
                        code { "ExecStartPre=/var/lib/nudo/self/nudo-boot-guard /var/lib/nudo/self" }
                        " and " code { "Environment=\"NUDO_SELF_DIR=/var/lib/nudo/self\"" }
                        " — and " code { "systemctl daemon-reload && systemctl restart nudo" } ". \
                         The packaged " code { "nudo.service" } " in the release archive \
                         already reads this way."
                    }
                    p .small.muted {
                        "The trade-off is real and worth knowing: in this layout the \
                         service user can overwrite its own binaries, which is what \
                         self-upgrading means. It stays inert until you turn the \
                         switch on in settings."
                    }
                }))
            }
        }
    }
}

/// The self-upgrade card for a managed binary install.
///
/// The one place in the dashboard that offers to perform an upgrade. The form
/// appears only when every gate is open and a newer release exists; otherwise
/// the card says which gate is closed and what opening it means, because a
/// button that is sometimes missing without explanation reads as a bug.
fn managed_upgrade(view: &UpgradeView, status: &SelfUpgradeView) -> Markup {
    let gates_open = status.enabled_in_settings;
    let in_flight = status.in_flight();
    html! {
        div .card {
            h2 { "This instance can upgrade itself" }
            div .card-body {
                p {
                    "It runs from a versioned release directory, so an upgrade is: \
                     download the release, verify it against the digest published \
                     in the manifest, stage it beside the running one, swap a \
                     symlink, and restart into it. If the new version cannot \
                     start, the boot guard puts the old one back."
                }

                @if in_flight {
                    div #self-upgrade-status
                        hx-get="/upgrade/status"
                        hx-trigger="every 2s"
                        hx-swap="innerHTML" {
                        (self_upgrade_status_fragment(status))
                    }
                } @else if !gates_open {
                    (callout("info", "Switched off", html! {
                        p {
                            "Self-upgrade is off for this instance. Turn it on in "
                            a href="/settings#instance" { "settings" }
                            " and this card grows a button whenever a release is out."
                        }
                    }))
                } @else if view.available {
                    form method="post" action="/upgrade/start" {
                        (csrf_input(&view.csrf))
                        input type="hidden" name="target_version" value=(view.latest);
                        div .form-actions {
                            button .btn.primary type="submit" {
                                "Upgrade to " (view.latest)
                            }
                        }
                    }
                    p .small.muted {
                        "The dashboard will lose the control plane for a moment while \
                         it restarts; this page keeps polling and picks the new \
                         version up when it comes back."
                    }
                } @else {
                    p .muted { "Nothing to do: this is the newest release." }
                }

                @if !in_flight && !status.error.is_empty() {
                    (callout("bad", &format!("The last attempt: {}", status.state), html! {
                        p { (status.error) }
                    }))
                }

                (callout("warn", "If it goes wrong", html! {
                    p {
                        "A new version that cannot start is put back automatically: \
                         after " (nudo_bootguard_attempts()) " failed starts the boot \
                         guard reverts to the previous release, which is kept on disk."
                    }
                    p {
                        "The database is snapshotted into the new release directory \
                         before the swap (" code { "db-pre-upgrade.sqlite" } "). \
                         Restoring it is deliberately manual — an automatic restore \
                         would silently discard anything written after the snapshot. \
                         To go back to it: stop nudo, copy the snapshot over the \
                         database, start nudo."
                    }
                }))
            }
        }
    }
}

/// The bootguard's attempt limit, printed rather than hard-coded in copy.
fn nudo_bootguard_attempts() -> u32 {
    nudo_bootguard::MAX_BOOT_ATTEMPTS
}

/// The polled fragment while an upgrade is in flight.
///
/// Also served standalone at `/upgrade/status`. The interesting render is the
/// unreachable one — see `self_upgrade_restarting` — because a dead control
/// plane mid-upgrade is not an error, it is the plan working.
pub fn self_upgrade_status_fragment(status: &SelfUpgradeView) -> Markup {
    // The named steps of an upgrade, in order, so progress reads as "three of
    // five" rather than as an opaque spinner.
    const STEPS: [(&str, &str); 5] = [
        ("downloading", "Downloading the release"),
        ("verifying", "Verifying the digest"),
        ("staging", "Staging the new version"),
        ("staged", "Snapshotting the database"),
        ("swapped", "Restarting into the new version"),
    ];
    let position = STEPS.iter().position(|(state, _)| *state == status.state);

    html! {
        @if let Some(position) = position {
            div .upgrade-steps {
                @for (index, (_, label)) in STEPS.iter().enumerate() {
                    div .upgrade-step.done[index < position].current[index == position] {
                        // The step being worked spins rather than showing its
                        // number: between two polls a static list cannot say
                        // whether anything is still happening, and "stuck" and
                        // "working" look identical when both are a numeral.
                        @if index == position {
                            span .step-mark.spinner aria-hidden="true" {}
                        } @else {
                            span .step-mark {
                                @if index < position { "✓" } @else { (index + 1) }
                            }
                        }
                        span { (label) }
                    }
                }
            }
            p .small.muted {
                "Upgrading to " (status.to_version) ". This page keeps watching; \
                 the control plane restarts near the end, which is expected."
            }
        } @else if status.state == "confirmed" {
            (callout("ok", &format!("Now running {}", status.to_version), html! {
                p {
                    "The upgrade is done and the new version confirmed itself on \
                     boot. "
                    a href="/" { "Reload the dashboard" }
                    " to see it everywhere."
                }
            }))
        } @else if status.state == "exec-failed" || status.state == "rolled-back" {
            (callout("bad", "The upgrade was rolled back", html! {
                p { (status.error) }
                p .small.muted {
                    "The previous version is running and nothing was lost. "
                    a href="/upgrade" { "The upgrade page" }
                    " has the details and the manual route."
                }
            }))
        } @else if status.state == "failed" {
            (callout("bad", "The upgrade did not start", html! {
                p { (status.error) }
                p .small.muted { "Nothing was changed on disk." }
            }))
        }
    }
}

/// What the status endpoint says while the control plane is unreachable —
/// which, mid-upgrade, means the restart is happening.
///
/// Deliberately not an error: this is the one moment in the sequence where the
/// thing serving the page is being replaced, and saying "unreachable" would
/// read as a failure at exactly the point where things are going right.
pub fn self_upgrade_restarting() -> Markup {
    html! {
        div .upgrade-steps {
            div .upgrade-step.current {
                span .step-mark.spinner aria-hidden="true" {}
                span { "Restarting into the new version" }
            }
        }
        p .small.muted {
            "The control plane is restarting — this page will catch it when it \
             comes back."
        }
    }
}

/// What the upgrade page needs.
#[derive(Debug, Clone)]
pub struct UpgradeView {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub breaking: bool,
    pub install: UpgradeInstall,
    /// For the one form this page may render (the managed self-upgrade).
    pub csrf: String,
}

/// How this instance is installed, and anything the instructions need with it.
#[derive(Debug, Clone)]
pub enum UpgradeInstall {
    Container {
        image: &'static str,
    },
    /// A binary install predating the self-release layout: manual commands.
    BinaryLegacy,
    /// A binary install running from the self-release layout.
    BinaryManaged {
        status: SelfUpgradeView,
    },
}

/// The control plane's self-upgrade status, flattened out of the proto type so
/// this module keeps depending on views rather than the wire format.
#[derive(Debug, Clone, Default)]
pub struct SelfUpgradeView {
    pub state: String,
    pub from_version: String,
    pub to_version: String,
    pub error: String,
    pub enabled_in_settings: bool,
    pub eligible: bool,
}

impl SelfUpgradeView {
    /// Whether an upgrade is somewhere between "asked for" and "confirmed".
    pub fn in_flight(&self) -> bool {
        matches!(
            self.state.as_str(),
            "downloading" | "verifying" | "staging" | "staged" | "swapped" | "restarting"
        )
    }

    /// Whether the "Update now" button should appear: this install can do it,
    /// the operator opted in, and nothing is already running.
    pub fn can_upgrade_now(&self) -> bool {
        self.eligible && self.enabled_in_settings && !self.in_flight()
    }
}

/// What the update dialog needs.
#[derive(Debug, Clone, Default)]
pub struct UpdateDialog {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub breaking: bool,
    /// Release notes for `latest`, from the recorded manifest.
    pub notes: String,
    pub url: String,
    pub csrf: String,
    /// `None` on an install that cannot upgrade itself at all — a container,
    /// or a control plane that could not be reached.
    pub self_upgrade: Option<SelfUpgradeView>,
}

/// What the banner needs to render, flattened out of the control plane's
/// `UpdateStatus` so this module does not depend on the server crate.
#[derive(Debug, Clone, Default)]
pub struct UpdateBanner {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub breaking: bool,
    pub url: String,
}

/// One entry on the changelog page.
#[derive(Debug, Clone, Default)]
pub struct ChangelogEntry {
    pub version: String,
    pub published_at: String,
    pub notes: String,
    pub url: String,
    pub breaking: bool,
    /// Whether this is the version currently running.
    pub current: bool,
}

/// The "What's new" page: every release the manifest knows about, newest first.
pub fn changelog_page(entries: &[ChangelogEntry], current_version: &str) -> Markup {
    html! {
        (topbar("What's new", Some(&format!("running {current_version}")), html! {}))
        div .content {
            @if entries.is_empty() {
                (empty_state(
                    "No release notes yet",
                    "The release check has not run, could not reach the manifest, or is \
                     turned off. Nothing is wrong with this instance — it just does not \
                     know what else has been published.",
                    Some(("Settings", "/settings")),
                ))
            } @else {
                @for entry in entries {
                    div .card {
                        div .card-head {
                            h2 {
                                (entry.version)
                                @if entry.current {
                                    " " (badge("running", BadgeKind::Ok))
                                }
                                @if entry.breaking {
                                    " " (badge("manual steps", BadgeKind::Bad))
                                }
                            }
                            @if !entry.published_at.is_empty() {
                                span .small.muted { (entry.published_at) }
                            }
                        }
                        div .card-body {
                            @if entry.notes.is_empty() {
                                p .muted { "No notes for this release." }
                            } @else {
                                (release_notes(&entry.notes))
                            }
                            @if !entry.url.is_empty() {
                                p {
                                    a href=(entry.url) target="_blank" rel="noreferrer noopener" {
                                        "Full notes"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Renders release notes.
///
/// Notes come from a manifest fetched over the network, so they are untrusted
/// input. Rather than run them through a Markdown library and then have to trust
/// its HTML sanitiser, this handles the two things release notes actually use —
/// bullets and paragraphs — and renders everything else as escaped text. Maud
/// escapes each line, so no markup in the manifest can reach the page.
fn release_notes(notes: &str) -> Markup {
    let mut blocks: Vec<NoteBlock> = Vec::new();

    for line in notes.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(item) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            match blocks.last_mut() {
                Some(NoteBlock::List(items)) => items.push(item.to_string()),
                _ => blocks.push(NoteBlock::List(vec![item.to_string()])),
            }
            continue;
        }

        // A heading keeps its text but not its level: the page already has a
        // hierarchy and notes should not introduce a competing one.
        let text = line.trim_start_matches('#').trim();
        blocks.push(NoteBlock::Paragraph(text.to_string()));
    }

    html! {
        @for block in &blocks {
            @match block {
                NoteBlock::Paragraph(text) => p { (text) },
                NoteBlock::List(items) => ul {
                    @for item in items { li { (item) } }
                },
            }
        }
    }
}

enum NoteBlock {
    Paragraph(String),
    List(Vec<String>),
}

// ---------------------------------------------------------------------------
// Supporting the project
// ---------------------------------------------------------------------------

/// The "support this project" banner.
///
/// Shown at most once a calendar month, and only to someone who has actually
/// deployed with it — see `support::should_prompt`. The dismiss button says
/// "Maybe next time" because that is what it does; the permanent off-switch is
/// in settings, where someone looking for it will find it.
pub fn support_banner(csrf: &str, links: SupportLinkView<'_>) -> Markup {
    html! {
        div .callout.info.support-banner {
            strong { "nudo is free, and built by one person" }
            p {
                "If it is saving you the cost of a platform, sponsoring keeps it \
                 maintained. If money is not on the table, a star or a good bug \
                 report genuinely helps too."
            }
            div .form-actions {
                a .btn.small.primary href=(links.sponsor) target="_blank" rel="noreferrer noopener" {
                    "Sponsor"
                }
                a .btn.small href=(links.repository) target="_blank" rel="noreferrer noopener" {
                    "Star on GitHub"
                }
                a .btn.small href=(links.issues) target="_blank" rel="noreferrer noopener" {
                    "Report a bug"
                }
                form method="post" action="/support/dismiss" style="display:inline" {
                    (csrf_input(csrf))
                    button .btn.small.quiet type="submit" { "Maybe next time" }
                }
            }
        }
    }
}

/// The links the support banner points at, passed in so this module does not
/// hard-code URLs in two places.
#[derive(Debug, Clone, Copy)]
pub struct SupportLinkView<'a> {
    pub sponsor: &'a str,
    pub repository: &'a str,
    pub issues: &'a str,
    pub discussions: &'a str,
}
