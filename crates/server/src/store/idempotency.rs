use super::*;

impl Store {
    /// Records the result of an operation performed under an idempotency key.
    ///
    /// Returns the previously recorded result id when the key has been seen, so
    /// a retried deploy returns the original deployment rather than starting a
    /// second one.
    pub async fn check_idempotency(
        &self,
        key: &str,
        action: &str,
    ) -> anyhow::Result<Option<String>> {
        if key.trim().is_empty() {
            return Ok(None);
        }

        let existing: Option<(String,)> =
            sqlx::query_as("SELECT result_id FROM idempotency_keys WHERE key = ?1 AND action = ?2")
                .bind(key)
                .bind(action)
                .fetch_optional(&self.pool)
                .await?;

        Ok(existing.map(|(id,)| id))
    }

    /// Stores the outcome of an operation against its idempotency key.
    pub async fn record_idempotency(
        &self,
        key: &str,
        action: &str,
        result_id: &str,
    ) -> anyhow::Result<()> {
        if key.trim().is_empty() {
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO idempotency_keys (key, action, result_id, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(action)
        .bind(result_id)
        .bind(now_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
