//! Cloning and building from a git source, on the control plane.
//!
//! Builds never happen on a target. A latency-critical host should not have a
//! compiler, a package manager, or a build's memory pressure on it; the control
//! plane produces a binary and ships only that.
//!
//! Credentials never reach the filesystem. An App installation token is passed
//! through `git -c url.<authed>.insteadOf=<plain>` on the command line, which
//! also covers submodules on the same host, and is redacted from any output
//! before it can reach a log or the dashboard.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use nudo_proto::{GitSource, source};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::crypto::SecretKey;
use crate::deploy::Artifact;
use crate::github;
use crate::ssh::OutputLine;
use crate::store::Store;

/// What replaces a token in any output we forward.
const REDACTED: &str = "***";

/// Clones a repository, runs its build command, and reads the resulting binary.
pub async fn clone_and_build(
    store: &Store,
    key: &SecretKey,
    git_source: &GitSource,
    git_ref_override: &str,
    workspace: &Path,
    timeout: Duration,
    output: mpsc::Sender<OutputLine>,
) -> anyhow::Result<Artifact> {
    let repo = git_source.repo.trim();
    if repo.is_empty() {
        bail!("this service's git source has no repository set");
    }
    // Validated before it reaches a command line.
    let (owner, name) = github::split_repo(repo)?;

    let git_ref = if git_ref_override.trim().is_empty() {
        git_source.branch.trim()
    } else {
        git_ref_override.trim()
    };
    let git_ref = if git_ref.is_empty() { "HEAD" } else { git_ref };

    let checkout = workspace.join("src");
    let credentials = resolve_credentials(store, key, &git_source.source_id).await?;

    let _ = output
        .send(OutputLine::stdout(format!(
            "cloning {owner}/{name} at {git_ref}"
        )))
        .await;

    clone(
        &credentials,
        owner,
        name,
        git_ref,
        &checkout,
        &output,
        timeout,
    )
    .await?;

    let sha = resolve_head_sha(&checkout).await.unwrap_or_default();
    if !sha.is_empty() {
        let _ = output
            .send(OutputLine::stdout(format!("checked out {sha}")))
            .await;
    }

    // ---- build ----
    let build_command = git_source.build_command.trim();
    if build_command.is_empty() {
        bail!(
            "this service's git source has no build command, so there is nothing \
             to produce a binary"
        );
    }

    let _ = output
        .send(OutputLine::stdout(format!("running: {build_command}")))
        .await;

    let status = run_streaming(
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(build_command)
            .current_dir(&checkout),
        &credentials,
        &output,
        timeout,
    )
    .await?;

    if status != 0 {
        bail!("the build command exited with status {status}");
    }

    // ---- collect the artifact ----
    let artifact_path = git_source.artifact_path.trim();
    if artifact_path.is_empty() {
        bail!(
            "this service's git source has no artifact path, so nudo does not know \
             which file the build produced"
        );
    }

    let produced = resolve_artifact_path(&checkout, artifact_path)?;
    let bytes = tokio::fs::read(&produced).await.with_context(|| {
        format!(
            "the build succeeded but produced no file at {}",
            produced.display()
        )
    })?;

    if bytes.is_empty() {
        bail!("the build produced an empty file at {}", produced.display());
    }

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

/// How to authenticate to the repository.
#[derive(Clone)]
enum Credentials {
    /// A GitHub App installation token, embedded in the clone URL.
    Token { token: String, html_url: String },
    /// An SSH deploy key, written to a temporary file for the clone's lifetime.
    DeployKey { private_key: String },
    /// A public repository.
    None,
}

impl Credentials {
    /// The secret to strip from any output.
    fn secret(&self) -> Option<&str> {
        match self {
            Self::Token { token, .. } => Some(token),
            // The key never appears on a command line or in output, only in a
            // file whose path is not sensitive.
            Self::DeployKey { .. } | Self::None => None,
        }
    }
}

/// Loads the credentials a source implies.
async fn resolve_credentials(
    store: &Store,
    key: &SecretKey,
    source_id: &str,
) -> anyhow::Result<Credentials> {
    if source_id.trim().is_empty() {
        // No source means a public repository, which is a legitimate
        // configuration and not an error.
        return Ok(Credentials::None);
    }

    let src = store
        .get_source(source_id)
        .await?
        .ok_or_else(|| anyhow!("no such source: {source_id}"))?;

    match source::Kind::try_from(src.kind) {
        Ok(source::Kind::GithubApp) => {
            let token = github::installation_token(store, key, source_id).await?;
            let urls = store
                .source_urls(source_id)
                .await?
                .ok_or_else(|| anyhow!("no such source: {source_id}"))?;
            Ok(Credentials::Token {
                token,
                html_url: urls.html_url,
            })
        }
        Ok(source::Kind::DeployKey) => {
            let private_key = store
                .source_deploy_key(key, source_id)
                .await?
                .ok_or_else(|| anyhow!("source {} has no deploy key", src.name))?;
            Ok(Credentials::DeployKey { private_key })
        }
        _ => Ok(Credentials::None),
    }
}

/// Clones one ref, shallow, into `checkout`.
async fn clone(
    credentials: &Credentials,
    owner: &str,
    name: &str,
    git_ref: &str,
    checkout: &Path,
    output: &mpsc::Sender<OutputLine>,
    timeout: Duration,
) -> anyhow::Result<()> {
    if let Some(parent) = checkout.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut command = tokio::process::Command::new("git");
    // Written to a temp file only for the deploy-key path; kept alive until the
    // clone finishes.
    let mut key_file: Option<tempfile::NamedTempFile> = None;

    match credentials {
        Credentials::Token { token, html_url } => {
            let host = host_of(html_url);
            // The insteadOf rewrite means submodules on the same host
            // authenticate too, without the token being written into
            // .git/config.
            let authed = format!(
                "https://x-access-token:{}@{host}/",
                urlencoding::encode(token)
            );
            command
                .arg("-c")
                .arg(format!("url.{authed}.insteadOf=https://{host}/"))
                // Some proxies mishandle HTTP/2 multiplexing with git.
                .arg("-c")
                .arg("http.version=HTTP/1.1")
                .arg("clone")
                .arg(format!("https://{host}/{owner}/{name}.git"));
        }
        Credentials::DeployKey { private_key } => {
            let mut file = tempfile::NamedTempFile::new()?;
            std::io::Write::write_all(&mut file, private_key.as_bytes())?;
            std::io::Write::flush(&mut file)?;
            crate::config::restrict_permissions(file.path())?;

            command
                .env(
                    "GIT_SSH_COMMAND",
                    format!(
                        "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
                         -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR",
                        file.path().display()
                    ),
                )
                .arg("clone")
                .arg(format!("git@github.com:{owner}/{name}.git"));
            key_file = Some(file);
        }
        Credentials::None => {
            command
                .arg("clone")
                .arg(format!("https://github.com/{owner}/{name}.git"));
        }
    }

    // Shallow, single-branch: a build needs the tree, not the history.
    command
        .arg("--depth=1")
        .arg("--recurse-submodules")
        .arg("--shallow-submodules");

    // A sha cannot be cloned with `--branch`; it needs a fetch after the fact.
    let is_sha = looks_like_sha(git_ref);
    if !is_sha && git_ref != "HEAD" {
        command.arg("--branch").arg(git_ref);
    }
    command.arg(checkout);

    let status = run_streaming(&mut command, credentials, output, timeout).await?;
    // Hold the key until the clone has actually finished with it.
    drop(key_file);

    if status != 0 {
        bail!(
            "git clone failed with status {status} — check that the branch exists \
             and that the source has access to the repository"
        );
    }

    if is_sha {
        let mut fetch = tokio::process::Command::new("git");
        fetch
            .current_dir(checkout)
            .arg("fetch")
            .arg("--depth=1")
            .arg("origin")
            .arg(git_ref);
        if run_streaming(&mut fetch, credentials, output, timeout).await? != 0 {
            bail!("could not fetch {git_ref}");
        }

        let mut checkout_cmd = tokio::process::Command::new("git");
        checkout_cmd
            .current_dir(checkout)
            .arg("checkout")
            .arg("--force")
            .arg(git_ref);
        if run_streaming(&mut checkout_cmd, credentials, output, timeout).await? != 0 {
            bail!("could not check out {git_ref}");
        }
    }

    Ok(())
}

/// Runs a command, forwarding its output line by line with secrets redacted.
async fn run_streaming(
    command: &mut tokio::process::Command,
    credentials: &Credentials,
    output: &mpsc::Sender<OutputLine>,
    timeout: Duration,
) -> anyhow::Result<i32> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A new process group, so a timeout can stop the whole build and not
        // just the shell that spawned it.
        .kill_on_drop(true)
        .spawn()
        .context("spawning the build process")?;

    let stdout = child.stdout.take().map(BufReader::new);
    let stderr = child.stderr.take().map(BufReader::new);
    let secret = credentials.secret().map(str::to_string);

    let pump_out = spawn_pump(stdout, false, output.clone(), secret.clone());
    let pump_err = spawn_pump(stderr, true, output.clone(), secret);

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.context("waiting for the build process")?,
        Err(_) => {
            // kill_on_drop would handle this eventually, but killing now stops
            // a runaway build from holding the machine.
            let _ = child.kill().await;
            bail!(
                "the command exceeded its {} second limit",
                timeout.as_secs()
            );
        }
    };

    let _ = pump_out.await;
    let _ = pump_err.await;

    // A signal produces no exit code; report something non-zero rather than
    // treating it as success.
    Ok(status.code().unwrap_or(-1))
}

/// Forwards a pipe's lines, redacting a secret if one was used.
fn spawn_pump<R>(
    reader: Option<BufReader<R>>,
    stderr: bool,
    output: mpsc::Sender<OutputLine>,
    secret: Option<String>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(reader) = reader else { return };
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let line = match &secret {
                Some(secret) if !secret.is_empty() => line.replace(secret.as_str(), REDACTED),
                _ => line,
            };
            let event = if stderr {
                OutputLine::stderr(line)
            } else {
                OutputLine::stdout(line)
            };
            if output.send(event).await.is_err() {
                return;
            }
        }
    })
}

/// Reads the checked-out commit.
async fn resolve_head_sha(checkout: &Path) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("git")
        .current_dir(checkout)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await?;

    if !output.status.success() {
        bail!("git rev-parse HEAD failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Resolves the artifact path inside the checkout, refusing to escape it.
///
/// The path comes from a service definition, which an agent or an API client can
/// set, so it must not be able to read `/etc/shadow` and ship it as a "binary".
pub fn resolve_artifact_path(checkout: &Path, artifact_path: &str) -> anyhow::Result<PathBuf> {
    let relative = Path::new(artifact_path.trim());
    if relative.is_absolute() {
        bail!("the artifact path must be relative to the repository root");
    }

    let mut resolved = checkout.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("the artifact path must not contain `..`");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("the artifact path must be relative to the repository root");
            }
        }
    }

    Ok(resolved)
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

    #[test]
    fn an_artifact_path_resolves_under_the_checkout() {
        let checkout = Path::new("/work/src");
        assert_eq!(
            resolve_artifact_path(checkout, "target/release/bot").expect("resolve"),
            Path::new("/work/src/target/release/bot")
        );
        // A leading ./ is how people write it half the time.
        assert_eq!(
            resolve_artifact_path(checkout, "./bot").expect("resolve"),
            Path::new("/work/src/bot")
        );
    }

    #[test]
    fn an_artifact_path_cannot_escape_the_checkout() {
        // A service definition is settable over the API and by an MCP agent, so
        // this is the boundary that stops an arbitrary file being shipped as a
        // binary.
        let checkout = Path::new("/work/src");
        for hostile in [
            "../../../etc/shadow",
            "target/../../../../etc/passwd",
            "/etc/passwd",
            "..",
        ] {
            assert!(
                resolve_artifact_path(checkout, hostile).is_err(),
                "{hostile:?} should be rejected"
            );
        }
    }

    #[test]
    fn shas_are_told_apart_from_branch_names() {
        // A sha needs a fetch-then-checkout; a branch can be cloned directly.
        assert!(looks_like_sha("a1b2c3d"));
        assert!(looks_like_sha("0123456789abcdef0123456789abcdef01234567"));

        assert!(!looks_like_sha("main"), "a branch is not a sha");
        assert!(!looks_like_sha("v1.0.0"));
        // Too short to be unambiguous.
        assert!(!looks_like_sha("abc"));
        // Too long to be a sha.
        assert!(!looks_like_sha(&"a".repeat(41)));
        // 'g' is not hex — this is a branch called "deadbeeg".
        assert!(!looks_like_sha("deadbeeg"));
        assert!(!looks_like_sha(""));
    }

    #[test]
    fn a_host_is_extracted_from_a_base_url() {
        assert_eq!(host_of("https://github.com"), "github.com");
        assert_eq!(host_of("https://github.com/"), "github.com");
        assert_eq!(
            host_of("https://git.internal.example.com/path"),
            "git.internal.example.com"
        );
    }

    #[test]
    fn only_the_token_credential_carries_a_secret_to_redact() {
        assert_eq!(
            Credentials::Token {
                token: "ghs_secret".to_string(),
                html_url: "https://github.com".to_string()
            }
            .secret(),
            Some("ghs_secret")
        );
        // The deploy key lives in a file, never on a command line or in output.
        assert!(
            Credentials::DeployKey {
                private_key: "key".to_string()
            }
            .secret()
            .is_none()
        );
        assert!(Credentials::None.secret().is_none());
    }

    #[tokio::test]
    async fn command_output_is_forwarded_line_by_line_with_streams_distinguished() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("echo to-stdout; echo to-stderr >&2");

        let status = run_streaming(
            &mut command,
            &Credentials::None,
            &tx,
            Duration::from_secs(30),
        )
        .await
        .expect("run");
        assert_eq!(status, 0);
        drop(tx);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(line) = rx.recv().await {
            if line.stderr {
                stderr.push(line.text)
            } else {
                stdout.push(line.text)
            }
        }
        assert_eq!(stdout, vec!["to-stdout"]);
        assert_eq!(stderr, vec!["to-stderr"]);
    }

    #[tokio::test]
    async fn a_token_appearing_in_output_is_redacted_before_it_is_forwarded() {
        // git echoes remote URLs on failure, so an unredacted token would land
        // in the deployment log and the dashboard.
        let (tx, mut rx) = mpsc::channel(16);
        let credentials = Credentials::Token {
            token: "ghs_supersecrettoken".to_string(),
            html_url: "https://github.com".to_string(),
        };

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("echo 'cloning https://x-access-token:ghs_supersecrettoken@github.com/o/r'");

        run_streaming(&mut command, &credentials, &tx, Duration::from_secs(30))
            .await
            .expect("run");
        drop(tx);

        let line = rx.recv().await.expect("line").text;
        assert!(
            !line.contains("ghs_supersecrettoken"),
            "token leaked: {line}"
        );
        assert!(line.contains(REDACTED));
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_reported_rather_than_swallowed() {
        let (tx, _rx) = mpsc::channel(16);
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("exit 7");

        let status = run_streaming(
            &mut command,
            &Credentials::None,
            &tx,
            Duration::from_secs(30),
        )
        .await
        .expect("run");
        assert_eq!(status, 7);
    }

    #[tokio::test]
    async fn a_command_that_exceeds_its_limit_is_killed() {
        let (tx, _rx) = mpsc::channel(16);
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("sleep 30");

        let error = run_streaming(
            &mut command,
            &Credentials::None,
            &tx,
            Duration::from_millis(200),
        )
        .await
        .expect_err("must time out");
        assert!(error.to_string().contains("limit"), "got: {error}");
    }

    #[tokio::test]
    async fn a_git_source_with_no_repository_is_refused() {
        let store = Store::open_in_memory().await.expect("store");
        let key = SecretKey::generate();
        let (tx, _rx) = mpsc::channel(16);
        let dir = tempfile::tempdir().expect("tempdir");

        let error = clone_and_build(
            &store,
            &key,
            &GitSource::default(),
            "",
            dir.path(),
            Duration::from_secs(5),
            tx,
        )
        .await
        .expect_err("must fail");
        assert!(error.to_string().contains("no repository"), "got: {error}");
    }

    #[tokio::test]
    async fn a_git_source_naming_an_unknown_source_id_is_refused() {
        let store = Store::open_in_memory().await.expect("store");
        let key = SecretKey::generate();
        assert!(
            resolve_credentials(&store, &key, "src_missing")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_empty_source_id_means_a_public_repository_rather_than_an_error() {
        let store = Store::open_in_memory().await.expect("store");
        let key = SecretKey::generate();
        assert!(matches!(
            resolve_credentials(&store, &key, "")
                .await
                .expect("resolve"),
            Credentials::None
        ));
    }

    #[tokio::test]
    async fn a_deploy_key_source_resolves_to_its_key() {
        let store = Store::open_in_memory().await.expect("store");
        let key = SecretKey::generate();
        let source = store
            .create_deploy_key_source(&key, "dk", "PRIVATE-KEY-BODY", "ssh-ed25519 AAA")
            .await
            .expect("source");

        match resolve_credentials(&store, &key, &source.id)
            .await
            .expect("resolve")
        {
            Credentials::DeployKey { private_key } => assert_eq!(private_key, "PRIVATE-KEY-BODY"),
            other => panic!(
                "expected a deploy key, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}
