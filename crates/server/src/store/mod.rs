//! SQLite state. One file, migrations applied on startup, no external database.
//!
//! Every row type is mapped explicitly rather than through `sqlx::query!`. The
//! compile-time macros need a populated database at build time, which would
//! make a clean checkout — and CI — unbuildable without a preparation step;
//! `sqlx::query_as` with `FromRow` keeps `cargo build` self-contained. The
//! trade-off is noted in CHANGES.md.

mod audit;
mod auth;
mod build_hosts;
mod deployments;
mod idempotency;
mod key_verifier;
mod secrets;
mod services;
mod settings;
mod sources;
mod targets;
mod terminals;

pub use audit::*;
pub use auth::*;
pub use build_hosts::*;
pub use deployments::*;
// `secrets` and `services` only add `impl Store` methods, which are in scope
// through `Store` itself; there are no types of their own to re-export.
pub use sources::*;
pub use targets::*;
pub use terminals::*;

use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Pool, Sqlite};

/// Handle to the control plane's state.
#[derive(Clone)]
pub struct Store {
    pool: Pool<Sqlite>,
}

impl Store {
    /// Opens (creating if absent) the database at `path` and applies migrations.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL lets the deploy engine write progress while the dashboard
            // reads, instead of the two blocking each other.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            // Rather than failing immediately on a locked database, wait —
            // contention here is short-lived by construction.
            .busy_timeout(std::time::Duration::from_secs(10));

        Self::from_options(options).await
    }

    /// Opens a private in-memory database. Used by tests, which each get their
    /// own isolated state.
    pub async fn open_in_memory() -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        Self::from_options(options).await
    }

    async fn from_options(options: SqliteConnectOptions) -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            // SQLite serializes writers anyway, and an in-memory database is
            // per-connection, so a single connection keeps behaviour identical
            // between tests and production.
            .max_connections(1)
            .connect_with(options)
            .await
            .context("opening the SQLite database")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("applying migrations")?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    /// Copies the database to `dest` as a consistent snapshot.
    ///
    /// `VACUUM INTO` rather than a file copy: the database runs in WAL mode,
    /// so `nudo.db` alone is not the database — pages still live in the `-wal`
    /// file beside it. The snapshot taken before a self-upgrade is what makes
    /// a manual recovery possible if the new version's migrations leave the
    /// schema ahead of a rolled-back binary.
    pub async fn snapshot_database(&self, dest: &std::path::Path) -> anyhow::Result<()> {
        use anyhow::Context as _;

        let dest = dest
            .to_str()
            .context("the snapshot path is not valid UTF-8")?;
        // VACUUM INTO refuses to overwrite; a stale snapshot from an earlier
        // failed attempt would otherwise wedge every retry.
        if std::path::Path::new(dest).exists() {
            std::fs::remove_file(dest).context("removing the previous snapshot")?;
        }
        // The path cannot be bound as a parameter in a VACUUM statement, so it
        // is embedded — with quotes escaped the SQL way. The caller composes
        // the path from the data directory and a version, never from input,
        // which is what the AssertSqlSafe below asserts.
        let escaped = dest.replace('\'', "''");
        sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{escaped}'")))
            .execute(&self.pool)
            .await
            .context("snapshotting the database")?;
        Ok(())
    }
}

/// Current UTC time in the text form used by every timestamp column.
pub fn now_string() -> String {
    to_db_time(chrono::Utc::now())
}

/// Formats a timestamp for storage. RFC 3339 with a fixed number of subsecond
/// digits, so lexicographic ordering matches chronological ordering.
pub fn to_db_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Parses a stored timestamp. Unparseable values yield `None` rather than
/// failing the whole query — a single bad row should not make a list endpoint
/// unusable.
pub fn from_db_time(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Parses a stored optional timestamp.
pub fn from_db_time_opt(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    raw.and_then(from_db_time)
}

/// Generates a prefixed identifier, e.g. `tgt_a1b2c3...`.
///
/// Prefixes make ids self-describing in logs and in the audit trail, so a
/// subject id alone tells you what kind of thing it refers to.
pub fn new_id(prefix: &str) -> String {
    let uuid = uuid::Uuid::new_v4();
    let short: String = uuid.simple().to_string().chars().take(20).collect();
    format!("{prefix}_{short}")
}

/// Serializes a string map for a JSON column.
pub fn encode_map(map: &std::collections::HashMap<String, String>) -> String {
    // BTreeMap so the stored JSON is stable — otherwise an unchanged update
    // rewrites the column and looks like a modification.
    let ordered: std::collections::BTreeMap<_, _> = map.iter().collect();
    serde_json::to_string(&ordered).unwrap_or_else(|_| "{}".to_string())
}

/// Deserializes a JSON string map, tolerating a corrupt value.
pub fn decode_map(raw: &str) -> std::collections::HashMap<String, String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Serializes a string list for a JSON column.
pub fn encode_list(list: &[String]) -> String {
    serde_json::to_string(list).unwrap_or_else(|_| "[]".to_string())
}

/// Deserializes a JSON string list, tolerating a corrupt value.
pub fn decode_list(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Clamps a requested page size into a sane range.
pub fn page_size(requested: u32) -> i64 {
    match requested {
        0 => 50,
        n => n.min(500) as i64,
    }
}

/// Decodes an opaque page token into an offset.
///
/// The token is just a stringified offset; it is opaque to clients by
/// convention rather than by encryption, and an unparseable token starts from
/// the beginning instead of erroring.
pub fn page_offset(token: &str) -> i64 {
    token.trim().parse().unwrap_or(0)
}

/// Builds the next page token, or an empty string when the page was the last.
pub fn next_page_token(offset: i64, returned: usize, limit: i64) -> String {
    if (returned as i64) < limit {
        String::new()
    } else {
        (offset + limit).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_in_memory_store_has_its_migrations_applied() {
        let store = Store::open_in_memory().await.expect("open");
        // Every table the schema declares must exist.
        for table in [
            "targets",
            "build_hosts",
            "build_defaults",
            "services",
            "releases",
            "deployments",
            "deployment_logs",
            "secrets",
            "sources",
            "github_setup_states",
            "github_installation_tokens",
            "users",
            "sessions",
            "api_tokens",
            "terminal_sessions",
            "audit_entries",
            "idempotency_keys",
            "self_upgrade_settings",
        ] {
            let found: Option<(String,)> =
                sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")
                    .bind(table)
                    .fetch_optional(store.pool())
                    .await
                    .expect("query");
            assert!(found.is_some(), "table {table} is missing");
        }
    }

    #[tokio::test]
    async fn opening_a_file_database_twice_reapplies_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("nudo.db");

        // The parent directory does not exist yet — open must create it.
        let store = Store::open(&path).await.expect("first open");
        drop(store);
        // Re-opening an already-migrated database must succeed.
        Store::open(&path).await.expect("second open");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn the_first_process_to_open_a_database_records_its_key() {
        let store = Store::open_in_memory().await.expect("open");
        let key = crate::crypto::SecretKey::generate();

        store
            .verify_secret_key(&key)
            .await
            .expect("first use seals a verifier");
        // And the same key still passes afterwards.
        store
            .verify_secret_key(&key)
            .await
            .expect("the same key still matches");
    }

    #[tokio::test]
    async fn a_different_key_is_refused_with_an_actionable_message() {
        // The footgun this exists for: running the control plane and the
        // dashboard with different keys starts both happily and then fails as
        // "corrupt ciphertext" while opening a terminal.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nudo.db");

        let original = crate::crypto::SecretKey::generate();
        {
            let store = Store::open(&path).await.expect("open");
            store.verify_secret_key(&original).await.expect("seal");
        }

        let store = Store::open(&path).await.expect("reopen");
        let error = store
            .verify_secret_key(&crate::crypto::SecretKey::generate())
            .await
            .expect_err("a different key must be refused");

        let message = error.to_string();
        assert!(message.contains("does not match"), "got: {message}");
        // The message has to say what to do about it.
        assert!(message.contains("NUDO_SECRET_KEY"), "got: {message}");
        assert!(message.contains("both need the same key"), "got: {message}");

        // The original key still works, so the check is not destructive.
        store
            .verify_secret_key(&original)
            .await
            .expect("the original key still matches");
    }

    #[tokio::test]
    async fn an_idempotency_key_returns_the_original_result_on_replay() {
        let store = Store::open_in_memory().await.expect("open");

        assert_eq!(
            store
                .check_idempotency("key-1", "Deploy")
                .await
                .expect("check"),
            None
        );
        store
            .record_idempotency("key-1", "Deploy", "dep_first")
            .await
            .expect("record");
        assert_eq!(
            store
                .check_idempotency("key-1", "Deploy")
                .await
                .expect("check"),
            Some("dep_first".to_string())
        );

        // Recording again must not overwrite the original outcome.
        store
            .record_idempotency("key-1", "Deploy", "dep_second")
            .await
            .expect("record");
        assert_eq!(
            store
                .check_idempotency("key-1", "Deploy")
                .await
                .expect("check"),
            Some("dep_first".to_string())
        );
    }

    #[tokio::test]
    async fn an_idempotency_key_is_scoped_to_its_action() {
        let store = Store::open_in_memory().await.expect("open");
        store
            .record_idempotency("shared", "Deploy", "dep_1")
            .await
            .expect("record");
        // The same key under a different action must not collide.
        assert_eq!(
            store
                .check_idempotency("shared", "Rollback")
                .await
                .expect("check"),
            None
        );
    }

    #[tokio::test]
    async fn an_empty_idempotency_key_is_ignored() {
        let store = Store::open_in_memory().await.expect("open");
        store
            .record_idempotency("", "Deploy", "x")
            .await
            .expect("record");
        assert_eq!(
            store.check_idempotency("", "Deploy").await.expect("check"),
            None
        );
    }

    #[test]
    fn stored_timestamps_sort_chronologically_as_text() {
        let earlier = to_db_time(
            chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .expect("parse")
                .into(),
        );
        let later = to_db_time(
            chrono::DateTime::parse_from_rfc3339("2026-11-30T23:59:59Z")
                .expect("parse")
                .into(),
        );
        // ORDER BY on the text column must be chronological.
        assert!(earlier < later);
    }

    #[test]
    fn timestamps_round_trip_through_their_stored_form() {
        let now = chrono::Utc::now();
        let restored = from_db_time(&to_db_time(now)).expect("parse");
        assert_eq!(now.timestamp(), restored.timestamp());
    }

    #[test]
    fn a_corrupt_timestamp_yields_none_rather_than_failing() {
        assert!(from_db_time("not a time").is_none());
        assert!(from_db_time_opt(None).is_none());
    }

    #[test]
    fn ids_are_prefixed_and_unique() {
        let a = new_id("tgt");
        let b = new_id("tgt");
        assert!(a.starts_with("tgt_"));
        assert_ne!(a, b);
    }

    #[test]
    fn maps_encode_in_a_stable_order() {
        // Otherwise an unchanged update rewrites the column.
        let map = std::collections::HashMap::from([
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string()),
        ]);
        let encoded = encode_map(&map);
        assert_eq!(encoded, r#"{"a":"2","z":"1"}"#);
        assert_eq!(decode_map(&encoded), map);
    }

    #[test]
    fn corrupt_json_columns_decode_to_empty_rather_than_failing() {
        assert!(decode_map("not json").is_empty());
        assert!(decode_list("not json").is_empty());
    }

    #[test]
    fn lists_round_trip() {
        let list = vec!["a".to_string(), "b".to_string()];
        assert_eq!(decode_list(&encode_list(&list)), list);
    }

    #[test]
    fn page_sizes_are_clamped_and_defaulted() {
        assert_eq!(page_size(0), 50, "unset means the default");
        assert_eq!(page_size(10), 10);
        assert_eq!(page_size(10_000), 500, "a huge page size is capped");
    }

    #[test]
    fn page_tokens_carry_the_offset_and_end_the_sequence_on_a_short_page() {
        assert_eq!(page_offset(""), 0);
        assert_eq!(page_offset("100"), 100);
        assert_eq!(
            page_offset("garbage"),
            0,
            "a bad token restarts rather than errors"
        );

        // A full page means there may be more.
        assert_eq!(next_page_token(0, 50, 50), "50");
        // A short page is the last one.
        assert_eq!(next_page_token(50, 12, 50), "");
    }
}
