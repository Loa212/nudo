use std::time::Duration;

use anyhow::{Context, bail};
use nudo_proto::{Service, artifact_source, deployment};
use tokio::sync::mpsc;

use crate::git;
use crate::ssh::OutputLine;

use super::{Artifact, DeployOptions, Engine};

/// How long to allow a build to run before giving up.
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How long to allow an artifact download to take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

impl Engine {
    /// Fetches, builds or reads the artifact this deploy should ship.
    pub(super) async fn obtain_artifact(
        &self,
        deployment_id: &str,
        service: &Service,
        options: &DeployOptions,
    ) -> anyhow::Result<Artifact> {
        // An explicit URL on the request wins, so a CI job can push a specific
        // build to a service that is otherwise git-backed.
        if !options.artifact_url.trim().is_empty() {
            return self
                .download_artifact(deployment_id, options.artifact_url.trim())
                .await;
        }

        if let Some(path) = &options.uploaded_artifact {
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("reading the uploaded artifact {}", path.display()))?;
            self.emit(
                deployment_id,
                &format!("using uploaded artifact ({} bytes)", bytes.len()),
                false,
            )
            .await;
            return Ok(Artifact {
                bytes,
                git_sha: String::new(),
                git_ref: String::new(),
            });
        }

        match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
            Some(artifact_source::Kind::Url(url)) if !url.trim().is_empty() => {
                self.download_artifact(deployment_id, url.trim()).await
            }
            Some(artifact_source::Kind::Git(git_source)) => {
                self.transition(deployment_id, deployment::Status::Building)
                    .await;
                self.build_from_git(deployment_id, git_source, &options.git_ref)
                    .await
            }
            _ => bail!(
                "this service has no artifact to deploy: give it a URL, connect a git \
                 source, or push a binary with `nudo deploy --artifact`"
            ),
        }
    }

    /// Downloads a prebuilt binary.
    pub(super) async fn download_artifact(
        &self,
        deployment_id: &str,
        url: &str,
    ) -> anyhow::Result<Artifact> {
        self.emit(deployment_id, &format!("downloading {url}"), false)
            .await;

        // Only http(s): without this a `file://` URL would make the control
        // plane read its own filesystem and ship the result to a target.
        let scheme = url.split("://").next().unwrap_or_default().to_lowercase();
        if scheme != "http" && scheme != "https" {
            bail!("artifact URLs must be http or https, not {scheme:?}");
        }

        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            // Bounded rather than disabled: a release asset URL redirects to
            // object storage, so following some hops is required. A chain this
            // long is a loop or a redirect-based probe.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?;

        let status = response.status();
        if !status.is_success() {
            bail!("downloading {url} returned HTTP {status}");
        }

        // A binary this large is a misconfiguration or a decompression bomb, and
        // it would be buffered in memory before ever reaching a target.
        const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
        if let Some(length) = response.content_length()
            && length > MAX_ARTIFACT_BYTES
        {
            bail!(
                "{url} is {length} bytes, over the {MAX_ARTIFACT_BYTES} byte limit \
                 for a deployable artifact"
            );
        }

        let bytes = response
            .bytes()
            .await
            .context("reading the artifact body")?;
        if bytes.is_empty() {
            bail!("{url} returned an empty artifact");
        }

        Ok(Artifact {
            bytes: bytes.to_vec(),
            git_sha: String::new(),
            git_ref: String::new(),
        })
    }

    /// Clones and builds from a connected git source, on the control plane.
    ///
    /// Building here rather than on the target is deliberate: a latency-critical
    /// host must not run a compiler.
    pub(super) async fn build_from_git(
        &self,
        deployment_id: &str,
        git_source: &nudo_proto::GitSource,
        git_ref_override: &str,
    ) -> anyhow::Result<Artifact> {
        let workspace = self.config.workspace_dir().join(deployment_id);
        tokio::fs::create_dir_all(&workspace)
            .await
            .with_context(|| format!("creating {}", workspace.display()))?;

        let engine = self.clone();
        let deployment_for_output = deployment_id.to_string();
        let (tx, mut rx) = mpsc::channel::<OutputLine>(256);
        let pump = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                engine
                    .emit(&deployment_for_output, &line.text, line.stderr)
                    .await;
            }
        });

        let result = git::clone_and_build(
            &self.store,
            &self.secret_key,
            git_source,
            git_ref_override,
            &workspace,
            BUILD_TIMEOUT,
            tx.clone(),
        )
        .await;

        drop(tx);
        let _ = pump.await;

        // The checkout can be large; a failed build should not leave it behind.
        let cleanup = tokio::fs::remove_dir_all(&workspace).await;
        if let Err(error) = cleanup {
            tracing::debug!(%error, path = %workspace.display(), "could not clean the workspace");
        }

        result
    }
}
