//! Runtime configuration, assembled from CLI flags and the environment.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

/// Configuration for the control plane process.
///
/// Everything has a working default so `nudo-server` runs with no arguments on
/// a fresh VPS; the only value most operators must set is `--secret-key` (or
/// `NUDO_SECRET_KEY`), since a generated-per-boot key would make previously
/// stored secrets unreadable after a restart.
#[derive(Parser, Debug, Clone)]
#[command(name = "nudo-server", about = "nudo control plane (gRPC API + deploy engine)")]
pub struct Config {
    /// Address the gRPC API listens on.
    #[arg(long, env = "NUDO_GRPC_ADDR", default_value = "127.0.0.1:50051")]
    pub grpc_addr: SocketAddr,

    /// Path to the SQLite database file. Created if absent.
    #[arg(long, env = "NUDO_DB", default_value = "nudo.db")]
    pub database: PathBuf,

    /// Directory for build workspaces, uploaded artifacts and generated keys.
    #[arg(long, env = "NUDO_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// 32-byte AES-256-GCM key for the secret store, hex or base64 encoded.
    /// Read from a file instead with `--secret-key-file`.
    #[arg(long, env = "NUDO_SECRET_KEY")]
    pub secret_key: Option<String>,

    /// File containing the secret-store key. Takes precedence over
    /// `--secret-key` so a systemd unit can point at a `0600` file rather than
    /// exposing the key in the process environment.
    #[arg(long, env = "NUDO_SECRET_KEY_FILE")]
    pub secret_key_file: Option<PathBuf>,

    /// Public base URL of this control plane. Used to build GitHub App webhook
    /// and callback URLs, so it must be reachable from GitHub.
    #[arg(long, env = "NUDO_BASE_URL", default_value = "http://localhost:3000")]
    pub base_url: String,

    /// How many log lines to retain per service in the in-memory ring buffer
    /// that backfills a freshly opened log view.
    #[arg(long, env = "NUDO_LOG_BUFFER", default_value_t = 2000)]
    pub log_buffer_lines: usize,

    /// Seconds between reachability probes of each target.
    #[arg(long, env = "NUDO_PROBE_INTERVAL", default_value_t = 60)]
    pub probe_interval_seconds: u64,

    /// Allow the first request to create the initial admin account when no
    /// user exists. Disable on a host where the dashboard is publicly exposed
    /// before you have had a chance to complete setup.
    #[arg(long, env = "NUDO_ALLOW_SETUP", default_value_t = true)]
    pub allow_setup: bool,
}

impl Config {
    /// Resolves the secret-store key from the file, the flag, or (as a last
    /// resort) generates one and persists it under the data directory.
    ///
    /// Generating is a convenience for a first run, not a security posture: the
    /// key is written to `<data_dir>/secret.key` with `0600` so a restart can
    /// still decrypt. Operators are told about it at warn level.
    pub fn resolve_secret_key(&self) -> anyhow::Result<crate::crypto::SecretKey> {
        use anyhow::Context;

        if let Some(path) = &self.secret_key_file {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading secret key file {}", path.display()))?;
            return crate::crypto::SecretKey::parse(raw.trim());
        }

        if let Some(raw) = &self.secret_key {
            return crate::crypto::SecretKey::parse(raw.trim());
        }

        let generated = self.data_dir.join("secret.key");
        if generated.exists() {
            let raw = std::fs::read_to_string(&generated)
                .with_context(|| format!("reading {}", generated.display()))?;
            return crate::crypto::SecretKey::parse(raw.trim());
        }

        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating {}", self.data_dir.display()))?;
        let key = crate::crypto::SecretKey::generate();
        std::fs::write(&generated, key.to_hex())?;
        restrict_permissions(&generated)?;
        tracing::warn!(
            path = %generated.display(),
            "no secret key configured; generated one. Back this file up — \
             losing it makes every stored secret unrecoverable."
        );
        Ok(key)
    }

    /// Where build checkouts and uploaded artifacts are staged.
    pub fn workspace_dir(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    /// Where CLI-uploaded artifacts land before being shipped to a target.
    pub fn upload_dir(&self) -> PathBuf {
        self.data_dir.join("uploads")
    }
}

/// Tightens a file to owner-only read/write. A no-op on platforms without Unix
/// permission bits.
pub fn restrict_permissions(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50051".parse().expect("static addr"),
            database: PathBuf::from("nudo.db"),
            data_dir: PathBuf::from("./data"),
            secret_key: None,
            secret_key_file: None,
            base_url: "http://localhost:3000".to_string(),
            log_buffer_lines: 2000,
            probe_interval_seconds: 60,
            allow_setup: true,
        }
    }
}
