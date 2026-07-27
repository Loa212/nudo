use super::*;

// ---------------------------------------------------------------------------
// Updates and the changelog
//
// The banner and the "What's new" page. Both render data the control plane has
// already fetched — this module never makes a request of its own.
// ---------------------------------------------------------------------------

/// The banner shown when a newer release exists.
///
/// Renders nothing when the instance is current, so the caller can place it
/// unconditionally.
///
/// Deliberately not an "Update now" button. Coolify's equivalent downloads a
/// shell script over the network and runs it as root; for a tool holding every
/// target's SSH keys, that is a lot of trust in a URL, so upgrading here stays a
/// deliberate act on the host.
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
                    "Upgrading is a manual step on the host; nudo does not update itself."
                }
            }
            div .form-actions {
                a .btn.small.primary href="/upgrade" { "How to upgrade" }
                a .btn.small href="/changelog" { "What's new" }
                @if !status.url.is_empty() {
                    a .btn.small href=(status.url) target="_blank" rel="noreferrer noopener" {
                        "Release notes"
                    }
                }
            }
        }
    }
}

/// The upgrade instructions, for the way this instance is actually installed.
///
/// A page rather than a button. nudo does not upgrade itself: it holds the SSH
/// keys for every machine it manages, and a process that can rewrite its own
/// binary — or fetch and run a script as root, which is how the tool this was
/// modelled on does it — is a much larger thing to trust than one that tells
/// you what to type.
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

            @match view.install {
                UpgradeInstall::Container { image } => (container_upgrade(image, tag)),
                UpgradeInstall::Binary => (binary_upgrade(tag)),
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
            }
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
}

/// How this instance is installed, and anything the instructions need with it.
#[derive(Debug, Clone)]
pub enum UpgradeInstall {
    Container { image: &'static str },
    Binary,
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
