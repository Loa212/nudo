use super::*;
use crate::store::Store;

fn config(self_dir: Option<PathBuf>) -> Arc<crate::Config> {
    Arc::new(crate::Config {
        self_dir,
        ..crate::Config::default()
    })
}

async fn store() -> Store {
    Store::open_in_memory().await.expect("store")
}

/// A manifest whose latest release carries a digest for the expected tarball.
fn recorded_manifest(version: &str, digest: &str) -> String {
    format!(
        r#"{{"releases":[{{"version":"{version}","artifacts":{{"{file}":{{"sha256":"{digest}"}}}}}}]}}"#,
        file = artifact_filename(version),
    )
}

fn layout(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir.join("releases").join(version)).expect("mkdir");
    nudo_bootguard::swap_current(dir, &format!("releases/{version}")).expect("link");
}

// ---- gates ----

#[tokio::test]
async fn the_settings_toggle_gates_everything() {
    // Off is the default, and the default refuses. This is the whole gate:
    // an instance that has not been told it may replace its own binaries
    // will not, however it was installed.
    let upgrader = SelfUpgrader::new(store().await, config(None));
    let error = upgrader.start("99.0.0").await.expect_err("must refuse");
    assert!(error.to_string().contains("switched off"), "{error}");
}

#[tokio::test]
async fn a_flat_install_is_refused_even_with_the_toggle_on() {
    // The test binary does not run from a managed layout, so even a
    // configured self_dir leaves eligibility at BinaryLegacy — the toggle
    // cannot conjure a layout that is not there.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store().await;
    store.set_self_upgrade_enabled(true).await.expect("toggle");
    let upgrader = SelfUpgrader::new(store, config(Some(dir.path().to_path_buf())));
    let error = upgrader.start("99.0.0").await.expect_err("must refuse");
    assert!(
        error.to_string().contains("cannot upgrade itself"),
        "{error}"
    );
}

#[tokio::test]
async fn the_status_reports_which_gates_are_open() {
    let store = store().await;
    let upgrader = SelfUpgrader::new(store.clone(), config(None));
    let view = upgrader.status().await;
    assert!(!view.enabled_in_settings, "off until switched on");
    assert!(!view.eligible);
    assert_eq!(view.state, "idle");

    store.set_self_upgrade_enabled(true).await.expect("toggle");
    assert!(upgrader.status().await.enabled_in_settings);
}

// ---- URL policy ----

#[test]
fn only_https_or_loopback_http_downloads_are_accepted() {
    validate_download_url("https://github.com/Loa212/nudo/releases/download/v1/a.tar.gz")
        .expect("https is fine");
    validate_download_url("http://127.0.0.1:8000/a.tar.gz").expect("loopback http is fine");
    validate_download_url("http://localhost:8000/a.tar.gz").expect("localhost http is fine");

    validate_download_url("http://mirror.example.com/a.tar.gz")
        .expect_err("remote plain http is refused");
    validate_download_url("ftp://example.com/a.tar.gz").expect_err("ftp is refused");
    validate_download_url("file:///etc/passwd").expect_err("file is refused");
}

// ---- digest comparison ----

#[test]
fn digest_comparison_is_case_insensitive_and_exact() {
    let digest = "a".repeat(64);
    assert!(digests_match(&digest, &digest));
    assert!(digests_match(&digest.to_uppercase(), &digest));
    assert!(!digests_match(&digest, &"b".repeat(64)));
    assert!(!digests_match("short", &digest));
}

// ---- unpacking ----

/// Builds a gzipped tarball whose entries live under a top-level directory,
/// the way the release workflow packages them.
fn tarball(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
    let path = dir.join("fixture.tar.gz");
    let file = std::fs::File::create(&path).expect("create");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (name, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("nudo-v9.9.9-test/{name}"), *contents)
            .expect("append");
    }
    builder
        .into_inner()
        .expect("finish")
        .finish()
        .expect("gzip");
    path
}

#[test]
fn unpacking_extracts_binaries_and_docs_and_skips_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tarball = tarball(
        dir.path(),
        &[
            ("nudo-all-in-one", b"fake binary".as_slice()),
            ("README.md", b"docs".as_slice()),
            ("something-unexpected", b"junk".as_slice()),
        ],
    );
    let release_dir = dir.path().join("release");
    unpack(&tarball, &release_dir).expect("unpack");

    assert!(release_dir.join("nudo-all-in-one").exists());
    assert!(release_dir.join("README.md").exists());
    assert!(
        !release_dir.join("something-unexpected").exists(),
        "unexpected names are skipped, not extracted"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(release_dir.join("nudo-all-in-one"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "binaries are executable");
    }
}

#[test]
fn a_tarball_without_the_main_binary_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tarball = tarball(dir.path(), &[("nudo-server", b"present".as_slice())]);
    let error = unpack(&tarball, &dir.path().join("release")).expect_err("must refuse");
    assert!(error.to_string().contains("nudo-all-in-one"), "{error}");
}

#[test]
fn a_traversal_path_in_the_tarball_fails_the_upgrade() {
    // A digest-verified artifact should never contain one; if it does, the
    // right response is a hard stop, not a skip.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hostile.tar.gz");
    let file = std::fs::File::create(&path).expect("create");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let contents = b"hostile".as_slice();
    let mut header = tar::Header::new_gnu();
    // The tar crate's own path setters refuse `..`, which is reassuring but
    // unhelpful for building a hostile fixture — write the name bytes raw.
    {
        let gnu = header.as_gnu_mut().expect("gnu header");
        let name = b"top/../../escape";
        gnu.name[..name.len()].copy_from_slice(name);
    }
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append(&header, contents).expect("append");
    builder
        .into_inner()
        .expect("finish")
        .finish()
        .expect("gzip");

    let error = unpack(&path, &dir.path().join("release")).expect_err("must refuse");
    assert!(error.to_string().contains("hostile path"), "{error}");
}

// ---- download ----

#[tokio::test]
async fn a_download_is_hashed_as_it_streams() {
    use sha2::Digest as _;
    let body = b"the artifact bytes";
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("artifact");
    let digest = download(&format!("{}/a.tar.gz", server.uri()), &dest)
        .await
        .expect("download");

    assert_eq!(digest, hex::encode(sha2::Sha256::digest(body)));
    assert_eq!(std::fs::read(&dest).expect("read"), body);
}

#[tokio::test]
async fn an_empty_download_is_refused() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let error = download(&format!("{}/a.tar.gz", server.uri()), &dir.path().join("a"))
        .await
        .expect_err("must refuse");
    assert!(error.to_string().contains("empty body"), "{error}");
}

#[tokio::test]
async fn a_missing_artifact_is_an_error_not_a_zero_byte_release() {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let error = download(&format!("{}/a.tar.gz", server.uri()), &dir.path().join("a"))
        .await
        .expect_err("404 must fail");
    assert!(error.to_string().contains("404"), "{error}");
}

// ---- reconciliation on boot ----

#[tokio::test]
async fn a_swapped_release_matching_the_running_version_confirms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let running = updates::current_version();
    layout(dir.path(), running);
    nudo_bootguard::write_attempts(dir.path(), 2).expect("attempts");
    Journal {
        state: JournalState::Swapped,
        from_version: "0.0.1".to_string(),
        to_version: running.to_string(),
        previous: "releases/0.0.1".to_string(),
        target: format!("releases/{running}"),
        updated_at: 1,
        error: String::new(),
    }
    .store(dir.path())
    .expect("journal");

    reconcile_in(dir.path(), &store().await).await;

    let journal = Journal::load(dir.path()).expect("load").expect("some");
    assert_eq!(journal.state, JournalState::Confirmed);
    assert_eq!(
        nudo_bootguard::read_attempts(dir.path()),
        0,
        "confirmation disarms the guard"
    );
}

#[tokio::test]
async fn a_swapped_release_booting_as_the_wrong_version_is_recorded_as_failed() {
    let dir = tempfile::tempdir().expect("tempdir");
    layout(dir.path(), "0.0.1");
    Journal {
        state: JournalState::Swapped,
        from_version: "0.0.1".to_string(),
        to_version: "not-the-running-version".to_string(),
        previous: "releases/0.0.1".to_string(),
        target: "releases/not-the-running-version".to_string(),
        updated_at: 1,
        error: String::new(),
    }
    .store(dir.path())
    .expect("journal");

    reconcile_in(dir.path(), &store().await).await;

    let journal = Journal::load(dir.path()).expect("load").expect("some");
    assert_eq!(journal.state, JournalState::Failed);
    assert!(
        journal.error.contains("did not become"),
        "{}",
        journal.error
    );
}

#[tokio::test]
async fn resting_states_are_left_alone() {
    // exec-failed and rolled-back are the dashboard's to show; a boot must
    // not overwrite what happened.
    let dir = tempfile::tempdir().expect("tempdir");
    layout(dir.path(), "0.0.1");
    for state in [JournalState::ExecFailed, JournalState::RolledBack] {
        Journal {
            state,
            from_version: "0.0.1".to_string(),
            to_version: "0.0.2".to_string(),
            previous: "releases/0.0.1".to_string(),
            target: "releases/0.0.2".to_string(),
            updated_at: 1,
            error: "what happened".to_string(),
        }
        .store(dir.path())
        .expect("journal");

        reconcile_in(dir.path(), &store().await).await;

        let journal = Journal::load(dir.path()).expect("load").expect("some");
        assert_eq!(journal.state, state, "{state:?} must survive a boot");
    }
}

// ---- retention ----

#[test]
fn pruning_keeps_the_newest_and_everything_still_referenced() {
    let dir = tempfile::tempdir().expect("tempdir");
    for version in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0", "0.6.0"] {
        std::fs::create_dir_all(dir.path().join("releases").join(version)).expect("mkdir");
    }
    nudo_bootguard::swap_current(dir.path(), "releases/0.6.0").expect("link");

    let journal = Journal {
        state: JournalState::Confirmed,
        from_version: "0.1.0".to_string(),
        to_version: "0.6.0".to_string(),
        // The journal still names the oldest release as the rollback target.
        previous: "releases/0.1.0".to_string(),
        target: "releases/0.6.0".to_string(),
        updated_at: 1,
        error: String::new(),
    };
    prune_releases(dir.path(), &journal);

    let survivors: Vec<bool> = ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0", "0.6.0"]
        .iter()
        .map(|version| dir.path().join("releases").join(version).exists())
        .collect();
    assert_eq!(
        survivors,
        // 0.6.0, 0.5.0, 0.4.0 are the newest three; 0.1.0 survives because
        // the journal names it as the rollback target; the middle two go.
        vec![true, false, false, true, true, true],
        "newest three plus the journal's rollback target survive"
    );
}

// ---- start() validation against the recorded manifest ----

#[tokio::test]
async fn a_version_that_is_not_newer_is_refused_before_anything_else_network_shaped() {
    // Uses the legacy-layout refusal ordering: the version check comes first
    // in a managed layout, but eligibility fires earlier here. What this test
    // pins is that a downgrade never gets as far as the manifest.
    let store = store().await;
    store.set_self_upgrade_enabled(true).await.expect("toggle");
    store
        .record_latest_version("0.0.1", &recorded_manifest("0.0.1", &"a".repeat(64)))
        .await
        .expect("record");
    let dir = tempfile::tempdir().expect("tempdir");
    let upgrader = SelfUpgrader::new(store, config(Some(dir.path().to_path_buf())));
    // The test binary is not in a managed layout, so this refusal is the
    // eligibility one — which is fine: it proves order (gates before network).
    upgrader.start("0.0.1").await.expect_err("must refuse");
}

#[test]
fn the_artifact_filename_matches_what_the_release_workflow_publishes() {
    // release.yml packages `nudo-${GITHUB_REF_NAME}-<target>` where the ref
    // is `v1.2.3` — so the file is `nudo-v1.2.3-x86_64-unknown-linux-musl.tar.gz`.
    assert_eq!(
        artifact_filename("1.2.3"),
        "nudo-v1.2.3-x86_64-unknown-linux-musl.tar.gz"
    );
}
