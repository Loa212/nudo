//! Terminal session grants.
//!
//! A client never tells the websocket endpoint which host to reach. It presents
//! a token, and the server looks up the target itself. The token is
//! short-lived, single-use, scoped to one target, and stored only as a digest —
//! so a token in a log or a browser history cannot be redeemed later, and a
//! database read cannot mint access.
//!
//! Coolify's realtime service takes the opposite approach: the browser sends a
//! full `ssh` command line and the server validates only the target host,
//! leaving `-o ProxyCommand=` under client control. That is deliberately not
//! reproduced here.

use anyhow::bail;
use sqlx::Row;

use super::{Store, new_id, now_string, to_db_time};
use crate::crypto::{random_token, sha256_hex};

/// How long a grant stays redeemable. Long enough for the browser to open the
/// websocket, short enough that a leaked token is not useful.
const TERMINAL_TOKEN_TTL_SECONDS: i64 = 60;

/// A redeemed grant: what the server needs to open the PTY.
#[derive(Debug, Clone)]
pub struct TerminalGrant {
    pub id: String,
    pub target_id: String,
    pub initial_command: String,
    pub cols: u32,
    pub rows: u32,
}

impl Store {
    /// Issues a terminal grant, returning `(id, token, expires_at)`.
    pub async fn create_terminal_session(
        &self,
        target_id: &str,
        initial_command: &str,
        cols: u32,
        rows: u32,
    ) -> anyhow::Result<(String, String, chrono::DateTime<chrono::Utc>)> {
        if self.get_target(target_id).await?.is_none() {
            bail!("no such target: {target_id}");
        }

        let id = new_id("term");
        let token = random_token(32);
        let expires_at = chrono::Utc::now() + chrono::Duration::seconds(TERMINAL_TOKEN_TTL_SECONDS);

        // Zero would request a degenerate PTY, so unset values become a
        // conventional default the client then corrects on its first resize.
        let cols = if cols == 0 { 80 } else { cols.min(1000) };
        let rows = if rows == 0 { 24 } else { rows.min(1000) };

        sqlx::query(
            "INSERT INTO terminal_sessions
               (id, token_hash, target_id, initial_command, cols, rows,
                consumed_at, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)",
        )
        .bind(&id)
        .bind(sha256_hex(&token))
        .bind(target_id)
        .bind(initial_command.trim())
        .bind(cols)
        .bind(rows)
        .bind(to_db_time(expires_at))
        .bind(now_string())
        .execute(self.pool())
        .await?;

        Ok((id, token, expires_at))
    }

    /// Redeems a grant, atomically marking it consumed.
    ///
    /// The `consumed_at IS NULL` guard is inside the UPDATE, so two websockets
    /// racing on the same token cannot both win — the second matches no rows.
    pub async fn consume_terminal_session(
        &self,
        session_id: &str,
        token: &str,
    ) -> anyhow::Result<Option<TerminalGrant>> {
        if token.trim().is_empty() {
            return Ok(None);
        }

        let row = sqlx::query(
            "UPDATE terminal_sessions SET consumed_at = ?1
             WHERE id = ?2
               AND token_hash = ?3
               AND consumed_at IS NULL
               AND expires_at > ?4
             RETURNING id, target_id, initial_command, cols, rows",
        )
        .bind(now_string())
        .bind(session_id.trim())
        .bind(sha256_hex(token.trim()))
        .bind(now_string())
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|row| TerminalGrant {
            id: row.get("id"),
            target_id: row.get("target_id"),
            initial_command: row.get("initial_command"),
            cols: row.get::<i64, _>("cols") as u32,
            rows: row.get::<i64, _>("rows") as u32,
        }))
    }

    /// Removes consumed and expired grants. Called periodically so the table
    /// does not grow without bound.
    pub async fn sweep_terminal_sessions(&self) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "DELETE FROM terminal_sessions
             WHERE expires_at < ?1 OR consumed_at IS NOT NULL",
        )
        .bind(now_string())
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TargetInput;

    async fn fixture() -> (Store, String) {
        let store = Store::open_in_memory().await.expect("open");
        let target = store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        (store, target.id)
    }

    #[tokio::test]
    async fn a_grant_can_be_redeemed_once_and_yields_the_target() {
        let (store, target_id) = fixture().await;
        let (id, token, expires_at) = store
            .create_terminal_session(&target_id, "", 120, 40)
            .await
            .expect("create");

        assert!(expires_at > chrono::Utc::now());

        let grant = store
            .consume_terminal_session(&id, &token)
            .await
            .expect("consume")
            .expect("some");
        // The server learns the host from its own state, not from the client.
        assert_eq!(grant.target_id, target_id);
        assert_eq!(grant.cols, 120);
        assert_eq!(grant.rows, 40);

        assert!(
            store
                .consume_terminal_session(&id, &token)
                .await
                .expect("consume")
                .is_none(),
            "a grant must be single-use"
        );
    }

    #[tokio::test]
    async fn the_token_is_stored_only_as_a_digest() {
        let (store, target_id) = fixture().await;
        let (_, token, _) = store
            .create_terminal_session(&target_id, "", 80, 24)
            .await
            .expect("create");

        let (stored,): (String,) = sqlx::query_as("SELECT token_hash FROM terminal_sessions")
            .fetch_one(store.pool())
            .await
            .expect("query");
        assert_ne!(stored, token);
        assert_eq!(stored, sha256_hex(&token));
    }

    #[tokio::test]
    async fn a_wrong_token_or_wrong_session_id_is_rejected() {
        let (store, target_id) = fixture().await;
        let (id, token, _) = store
            .create_terminal_session(&target_id, "", 80, 24)
            .await
            .expect("create");

        assert!(
            store
                .consume_terminal_session(&id, "guessed")
                .await
                .expect("consume")
                .is_none()
        );
        assert!(
            store
                .consume_terminal_session("term_other", &token)
                .await
                .expect("consume")
                .is_none()
        );
        assert!(
            store
                .consume_terminal_session(&id, "")
                .await
                .expect("consume")
                .is_none()
        );

        // The real token still works, so a failed attempt does not burn it.
        assert!(
            store
                .consume_terminal_session(&id, &token)
                .await
                .expect("consume")
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_expired_grant_is_rejected() {
        let (store, target_id) = fixture().await;
        let (id, token, _) = store
            .create_terminal_session(&target_id, "", 80, 24)
            .await
            .expect("create");

        sqlx::query("UPDATE terminal_sessions SET expires_at = ?1")
            .bind(to_db_time(
                chrono::Utc::now() - chrono::Duration::seconds(1),
            ))
            .execute(store.pool())
            .await
            .expect("expire");

        assert!(
            store
                .consume_terminal_session(&id, &token)
                .await
                .expect("consume")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_grant_requires_an_existing_target() {
        let (store, _) = fixture().await;
        assert!(
            store
                .create_terminal_session("tgt_missing", "", 80, 24)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_initial_command_is_carried_through() {
        let (store, target_id) = fixture().await;
        let (id, token, _) = store
            .create_terminal_session(&target_id, "journalctl -f", 80, 24)
            .await
            .expect("create");

        let grant = store
            .consume_terminal_session(&id, &token)
            .await
            .expect("consume")
            .expect("some");
        assert_eq!(grant.initial_command, "journalctl -f");
    }

    #[tokio::test]
    async fn unset_dimensions_get_conventional_defaults_and_absurd_ones_are_capped() {
        let (store, target_id) = fixture().await;

        let (id, token, _) = store
            .create_terminal_session(&target_id, "", 0, 0)
            .await
            .expect("create");
        let grant = store
            .consume_terminal_session(&id, &token)
            .await
            .expect("consume")
            .expect("some");
        assert_eq!((grant.cols, grant.rows), (80, 24));

        let (id, token, _) = store
            .create_terminal_session(&target_id, "", 99_999, 99_999)
            .await
            .expect("create");
        let grant = store
            .consume_terminal_session(&id, &token)
            .await
            .expect("consume")
            .expect("some");
        assert_eq!((grant.cols, grant.rows), (1000, 1000));
    }

    #[tokio::test]
    async fn the_sweep_removes_consumed_and_expired_grants_but_not_live_ones() {
        let (store, target_id) = fixture().await;

        let (consumed_id, consumed_token, _) = store
            .create_terminal_session(&target_id, "", 80, 24)
            .await
            .expect("create");
        store
            .consume_terminal_session(&consumed_id, &consumed_token)
            .await
            .expect("consume");

        let (live_id, live_token, _) = store
            .create_terminal_session(&target_id, "", 80, 24)
            .await
            .expect("create");

        let removed = store.sweep_terminal_sessions().await.expect("sweep");
        assert_eq!(removed, 1);

        // The unredeemed grant survives and still works.
        assert!(
            store
                .consume_terminal_session(&live_id, &live_token)
                .await
                .expect("consume")
                .is_some()
        );
    }

    #[tokio::test]
    async fn deleting_a_target_removes_its_pending_grants() {
        let (store, target_id) = fixture().await;
        let (id, token, _) = store
            .create_terminal_session(&target_id, "", 80, 24)
            .await
            .expect("create");

        store.delete_target(&target_id).await.expect("delete");
        assert!(
            store
                .consume_terminal_session(&id, &token)
                .await
                .expect("consume")
                .is_none(),
            "a grant must not outlive its target"
        );
    }
}
