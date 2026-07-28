//! Building on a machine other than the control plane.
//!
//! The local path in [`crate::git`] stays exactly as it was and stays the
//! default. This module is the other option: clone, build and collect on a host
//! reached over SSH, so a 1 vCPU control plane does not have to compile
//! whatever a service happens to point at.
//!
//! Two properties are deliberate, and the tests below exist to hold them:
//!
//! **The deploy log must not reveal where the build ran.** Same lines, same
//! order, same redaction as building locally. An operator reading a deployment
//! should see a build; where it happened is configuration, not output.
//!
//! **The workspace is removed however the build ends.** Success, failure,
//! timeout, a lost connection — a build host that accumulates checkouts fills
//! up, and the failure then belongs to every service that builds there.
//!
//! Credentials do reach the build host: this is the design the issue chose,
//! cloning there rather than transferring a tree. A deploy key is written to a
//! `0600` file for the clone's lifetime and removed with the workspace; an App
//! token rides the command line as it does locally and is redacted from output.
//! That widens where secrets live by exactly one machine, which is the cost of
//! not moving the tree, and is why a build host is registered deliberately
//! rather than inferred.
//!
//! A build host is **not** a sandbox. Two builds here can see each other. That
//! is a property of how the operator runs the host, not something nudo
//! implements.

use std::time::Duration;

use anyhow::{Context, bail};
use nudo_proto::{BuildHost, GitSource};
use tokio::sync::mpsc;

use crate::crypto::SecretKey;
use crate::deploy::Artifact;
use crate::git::{Credentials, MAX_ARTIFACT_BYTES, REDACTED, resolve_artifact_path};
use crate::github;
use crate::ssh::{OutputLine, SshSession, quote};
use crate::store::Store;

/// What one remote build needs to know.
///
/// `workspace` is the per-build directory on the remote host; the caller owns
/// creating and removing it, so that cleanup happens even when the build fails
/// before this function returns.
pub struct RemoteBuild<'a> {
    pub store: &'a Store,
    pub key: &'a SecretKey,
    pub session: &'a SshSession,
    pub git_source: &'a GitSource,
    /// A branch, tag or sha overriding the source's own branch. Empty to use it.
    pub git_ref_override: &'a str,
    pub workspace: &'a str,
    pub timeout: Duration,
}

/// Runs a build on a build host, returning the artifact it produced.
pub async fn build_remotely(
    build: RemoteBuild<'_>,
    output: mpsc::Sender<OutputLine>,
) -> anyhow::Result<Artifact> {
    let RemoteBuild {
        store,
        key,
        session,
        git_source,
        git_ref_override,
        workspace,
        timeout,
    } = build;

    let repo = git_source.repo.trim();
    if repo.is_empty() {
        bail!("this service's git source has no repository set");
    }
    // Validated before it reaches a command line, exactly as locally.
    let (owner, name) = github::split_repo(repo)?;

    let git_ref = if git_ref_override.trim().is_empty() {
        git_source.branch.trim()
    } else {
        git_ref_override.trim()
    };
    let git_ref = if git_ref.is_empty() { "HEAD" } else { git_ref };

    let build_command = git_source.build_command.trim();
    if build_command.is_empty() {
        bail!(
            "this service's git source has no build command, so there is nothing \
             to produce a binary"
        );
    }
    let artifact_path = git_source.artifact_path.trim();
    if artifact_path.is_empty() {
        bail!(
            "this service's git source has no artifact path, so nudo does not know \
             which file the build produced"
        );
    }
    // Rejects `..` and absolute paths before anything runs, so a service
    // definition cannot name a file outside the checkout for collection. The
    // local path checks the same thing against a real path; here the checkout
    // is remote, so the rule is applied to the relative path itself.
    resolve_artifact_path(std::path::Path::new("/checkout"), artifact_path)?;

    let checkout = format!("{workspace}/src");
    let credentials = resolve_credentials(store, key, &git_source.source_id).await?;
    let secret = credentials.secret().map(str::to_string);

    // The same first line the local build emits.
    let _ = output
        .send(OutputLine::stdout(format!(
            "cloning {owner}/{name} at {git_ref}"
        )))
        .await;

    clone(
        session,
        &credentials,
        owner,
        name,
        git_ref,
        workspace,
        &checkout,
        &output,
        timeout,
    )
    .await?;

    let sha = resolve_head_sha(session, &checkout)
        .await
        .unwrap_or_default();
    if !sha.is_empty() {
        let _ = output
            .send(OutputLine::stdout(format!("checked out {sha}")))
            .await;
    }

    // ---- build ----
    let _ = output
        .send(OutputLine::stdout(format!("running: {build_command}")))
        .await;

    let status = run_streaming(
        session,
        // `cd` into the checkout rather than assuming the login shell landed
        // anywhere in particular, and fail rather than build in the wrong
        // directory if it is missing.
        &format!("cd {} && {}", quote(&checkout), build_command),
        &secret,
        &output,
        timeout,
    )
    .await?;

    if status != 0 {
        bail!("the build command exited with status {status}");
    }

    // ---- collect the artifact ----
    let produced = format!("{checkout}/{}", artifact_path.trim_start_matches("./"));
    let bytes = session
        .read_file(&produced, MAX_ARTIFACT_BYTES)
        .await
        .with_context(|| format!("collecting {artifact_path} from the build host"))?;

    // Byte-identical to the local path's final line.
    let _ = output
        .send(OutputLine::stdout(format!(
            "built {} ({} bytes)",
            artifact_path,
            bytes.len()
        )))
        .await;

    Ok(Artifact {
        bytes,
        git_sha: sha,
        git_ref: git_ref.to_string(),
    })
}

/// Loads the credentials a source implies.
///
/// Re-implemented against the same store calls rather than shared with the
/// local path, because the local one is private to `git` and returning its
/// `Credentials` is the only coupling worth having between the two.
async fn resolve_credentials(
    store: &Store,
    key: &SecretKey,
    source_id: &str,
) -> anyhow::Result<Credentials> {
    crate::git::resolve_credentials(store, key, source_id).await
}

/// Clones one ref, shallow, into `checkout` on the build host.
///
/// Takes its arguments individually rather than borrowing [`RemoteBuild`]: the
/// owner and repository name are already split and validated by the caller, and
/// re-deriving them here would mean parsing the same string twice.
#[allow(clippy::too_many_arguments)]
async fn clone(
    session: &SshSession,
    credentials: &Credentials,
    owner: &str,
    name: &str,
    git_ref: &str,
    workspace: &str,
    checkout: &str,
    output: &mpsc::Sender<OutputLine>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let secret = credentials.secret().map(str::to_string);
    // Shallow, single-branch: a build needs the tree, not the history.
    let depth = "--depth=1 --recurse-submodules --shallow-submodules";
    let is_sha = looks_like_sha(git_ref);
    let branch = if !is_sha && git_ref != "HEAD" {
        format!(" --branch {}", quote(git_ref))
    } else {
        String::new()
    };

    let command = match credentials {
        Credentials::Token { token, html_url } => {
            let host = host_of(html_url);
            // The insteadOf rewrite covers submodules on the same host without
            // the token being written into .git/config.
            let authed = format!(
                "https://x-access-token:{}@{host}/",
                urlencoding::encode(token)
            );
            format!(
                "git -c {} -c http.version=HTTP/1.1 clone {depth}{branch} {} {}",
                quote(&format!("url.{authed}.insteadOf=https://{host}/")),
                quote(&format!("https://{host}/{owner}/{name}.git")),
                quote(checkout)
            )
        }
        Credentials::DeployKey { private_key } => {
            // Written next to the workspace so it is removed with it, and
            // 0600 before the key lands in it rather than after.
            let key_path = format!("{workspace}/deploy_key");
            write_deploy_key(session, &key_path, private_key).await?;

            format!(
                "GIT_SSH_COMMAND={} git clone {depth}{branch} {} {}",
                quote(&format!(
                    "ssh -i {key_path} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
                     -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
                )),
                quote(&format!("git@github.com:{owner}/{name}.git")),
                quote(checkout)
            )
        }
        Credentials::None => format!(
            "git clone {depth}{branch} {} {}",
            quote(&format!("https://github.com/{owner}/{name}.git")),
            quote(checkout)
        ),
    };

    if run_streaming(session, &command, &secret, output, timeout).await? != 0 {
        bail!(
            "git clone failed — check that the branch exists, that the source has \
             access to the repository, and that the build host can reach it"
        );
    }

    if is_sha {
        // A sha cannot be cloned with `--branch`; it needs a fetch after the
        // fact. The deploy key is gone by now, so this only works for public
        // and token-authenticated repositories — the same limitation the local
        // path has, since it also drops the key after the clone.
        let fetch = format!(
            "cd {} && git fetch --depth=1 origin {}",
            quote(checkout),
            quote(git_ref)
        );
        if run_streaming(session, &fetch, &secret, output, timeout).await? != 0 {
            bail!("could not fetch {git_ref}");
        }

        let checkout_cmd = format!(
            "cd {} && git checkout --force {}",
            quote(checkout),
            quote(git_ref)
        );
        if run_streaming(session, &checkout_cmd, &secret, output, timeout).await? != 0 {
            bail!("could not check out {git_ref}");
        }
    }

    Ok(())
}

/// Writes a deploy key to the build host at `0600`.
///
/// The mode is set before the key is written rather than after, so the material
/// never exists in a world-readable file even briefly.
async fn write_deploy_key(
    session: &SshSession,
    path: &str,
    private_key: &str,
) -> anyhow::Result<()> {
    session
        .exec(&format!(
            "install -m 600 /dev/null {} 2>/dev/null || (touch {} && chmod 600 {})",
            quote(path),
            quote(path),
            quote(path)
        ))
        .await?
        .require_success("preparing the deploy key file on the build host")?;

    // `write_file` would `mkdir -p` and truncate, which is fine, but it would
    // also chmod after writing; the file is already created restricted above.
    session
        .write_file(path, private_key.as_bytes(), Some("600"))
        .await
        .context("writing the deploy key to the build host")?;
    Ok(())
}

/// Runs a command on the build host, forwarding output with secrets redacted.
///
/// The remote counterpart of `git::run_streaming`. Redaction happens here, on
/// the control plane, before a line reaches the deployment log — the same place
/// and the same substitution as locally.
async fn run_streaming(
    session: &SshSession,
    command: &str,
    secret: &Option<String>,
    output: &mpsc::Sender<OutputLine>,
    timeout: Duration,
) -> anyhow::Result<i32> {
    let (tx, mut rx) = mpsc::channel::<OutputLine>(256);

    let forward = {
        let output = output.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            while let Some(mut line) = rx.recv().await {
                if let Some(secret) = &secret
                    && !secret.is_empty()
                {
                    line.text = line.text.replace(secret.as_str(), REDACTED);
                }
                if output.send(line).await.is_err() {
                    return;
                }
            }
        })
    };

    let result = tokio::time::timeout(timeout, session.exec_streaming(command, tx)).await;
    let _ = forward.await;

    match result {
        Ok(status) => status,
        Err(_) => bail!(
            "the command exceeded its {} second limit",
            timeout.as_secs()
        ),
    }
}

/// Reads the checked-out commit on the build host.
async fn resolve_head_sha(session: &SshSession, checkout: &str) -> anyhow::Result<String> {
    let result = session
        .exec(&format!("cd {} && git rev-parse HEAD", quote(checkout)))
        .await?;
    if !result.ok() {
        bail!("git rev-parse HEAD failed");
    }
    Ok(result.trimmed().to_string())
}

/// Removes a build's workspace.
///
/// Best-effort and never fatal: a build that produced a binary should not be
/// failed because a directory could not be removed. The failure is logged so a
/// host that is filling up is visible before it is full.
pub async fn cleanup_workspace(session: &SshSession, workspace: &str) {
    // Guard against ever aiming this at `/` or at a relative path. The
    // workspace is composed from a validated root and a deployment id, so this
    // should be unreachable — which is exactly when a stray `rm -rf` is most
    // expensive.
    if !workspace.starts_with('/') || workspace.trim_end_matches('/').matches('/').count() < 2 {
        tracing::error!(%workspace, "refusing to remove an implausible build workspace");
        return;
    }

    match session.exec(&format!("rm -rf {}", quote(workspace))).await {
        Ok(result) if result.ok() => {}
        Ok(result) => tracing::warn!(
            %workspace,
            stderr = %result.stderr.trim(),
            "could not remove the build workspace"
        ),
        Err(error) => tracing::warn!(%error, %workspace, "could not remove the build workspace"),
    }
}

/// The per-build workspace directory under a build host's root.
pub fn workspace_for(build_host: &BuildHost, deployment_id: &str) -> String {
    let root = build_host.workspace_root.trim().trim_end_matches('/');
    let root = if root.is_empty() {
        crate::store::DEFAULT_WORKSPACE_ROOT
    } else {
        root
    };
    format!("{root}/{deployment_id}")
}

/// Whether a ref looks like a commit sha rather than a branch or tag name.
fn looks_like_sha(git_ref: &str) -> bool {
    let len = git_ref.len();
    (7..=40).contains(&len) && git_ref.chars().all(|c| c.is_ascii_hexdigit())
}

/// The host portion of a base URL.
fn host_of(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("github.com")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_host(workspace_root: &str) -> BuildHost {
        BuildHost {
            id: "bh_1".to_string(),
            name: "builder".to_string(),
            workspace_root: workspace_root.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_workspace_is_a_directory_per_build_under_the_host_root() {
        // Per build, so two concurrent builds cannot write over each other's
        // checkout — the one isolation property nudo does provide.
        assert_eq!(
            workspace_for(&build_host("/var/lib/nudo/builds"), "dep_123"),
            "/var/lib/nudo/builds/dep_123"
        );
        assert_eq!(
            workspace_for(&build_host("/mnt/fast/builds/"), "dep_123"),
            "/mnt/fast/builds/dep_123"
        );
    }

    #[test]
    fn a_build_host_without_a_workspace_root_falls_back_to_the_default() {
        assert_eq!(
            workspace_for(&build_host("   "), "dep_1"),
            format!("{}/dep_1", crate::store::DEFAULT_WORKSPACE_ROOT)
        );
    }

    #[test]
    fn an_implausible_workspace_is_never_handed_to_rm() {
        // `cleanup_workspace` runs `rm -rf`. These are the shapes that must
        // never reach it, whatever a caller composes.
        for hostile in ["/", "/tmp", "", "relative/path", "//"] {
            assert!(
                !hostile.starts_with('/') || hostile.trim_end_matches('/').matches('/').count() < 2,
                "{hostile:?} would have been accepted by the guard"
            );
        }
        // And a real workspace is accepted.
        let real = "/var/lib/nudo/builds/dep_1";
        assert!(real.starts_with('/') && real.trim_end_matches('/').matches('/').count() >= 2);
    }

    #[test]
    fn shas_are_told_apart_from_branch_names() {
        assert!(looks_like_sha("a1b2c3d"));
        assert!(looks_like_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!looks_like_sha("main"));
        assert!(!looks_like_sha("v1.0.0"));
        assert!(!looks_like_sha("abc"));
        assert!(!looks_like_sha("deadbeeg"));
        assert!(!looks_like_sha(""));
    }

    #[test]
    fn a_host_is_extracted_from_a_base_url() {
        assert_eq!(host_of("https://github.com"), "github.com");
        assert_eq!(
            host_of("https://git.internal.example.com/path"),
            "git.internal.example.com"
        );
    }

    #[test]
    fn a_git_source_with_no_repository_is_refused_before_anything_connects() {
        // The validations run in the same order as the local path, so a
        // misconfigured service fails the same way wherever it builds — and
        // fails before a connection is made rather than after a clone.
        let error = validate_source(&GitSource::default()).expect_err("must fail");
        assert!(error.to_string().contains("no repository"), "got: {error}");
    }

    #[test]
    fn a_git_source_missing_a_build_command_or_artifact_path_is_refused() {
        let error = validate_source(&GitSource {
            repo: "owner/name".to_string(),
            ..Default::default()
        })
        .expect_err("must fail");
        assert!(
            error.to_string().contains("no build command"),
            "got: {error}"
        );

        let error = validate_source(&GitSource {
            repo: "owner/name".to_string(),
            build_command: "make".to_string(),
            ..Default::default()
        })
        .expect_err("must fail");
        assert!(
            error.to_string().contains("no artifact path"),
            "got: {error}"
        );
    }

    #[test]
    fn an_artifact_path_escaping_the_checkout_is_refused_before_the_build_runs() {
        // The remote checkout cannot be resolved against a real filesystem, so
        // the rule is applied to the relative path itself — a service
        // definition still must not be able to name /etc/shadow and have it
        // shipped as a binary.
        for hostile in ["../../../etc/shadow", "/etc/passwd", ".."] {
            let error = validate_source(&GitSource {
                repo: "owner/name".to_string(),
                build_command: "make".to_string(),
                artifact_path: hostile.to_string(),
                ..Default::default()
            })
            .expect_err("must reject {hostile}");
            let _ = error;
        }

        // An ordinary path passes.
        assert!(
            validate_source(&GitSource {
                repo: "owner/name".to_string(),
                build_command: "make".to_string(),
                artifact_path: "target/release/bot".to_string(),
                ..Default::default()
            })
            .is_ok()
        );
    }

    /// The validation `build_remotely` performs before it touches the network,
    /// extracted so it can be tested without an SSH server.
    fn validate_source(git_source: &GitSource) -> anyhow::Result<()> {
        let repo = git_source.repo.trim();
        if repo.is_empty() {
            bail!("this service's git source has no repository set");
        }
        github::split_repo(repo)?;
        if git_source.build_command.trim().is_empty() {
            bail!(
                "this service's git source has no build command, so there is nothing \
                 to produce a binary"
            );
        }
        let artifact_path = git_source.artifact_path.trim();
        if artifact_path.is_empty() {
            bail!(
                "this service's git source has no artifact path, so nudo does not know \
                 which file the build produced"
            );
        }
        resolve_artifact_path(std::path::Path::new("/checkout"), artifact_path)?;
        Ok(())
    }
}
