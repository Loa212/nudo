use super::*;

impl Store {
    /// Confirms the secret-store key matches the one this database was written
    /// under, sealing a verifier on first use.
    ///
    /// The control plane and the dashboard are separate processes sharing a
    /// database and a key. Without this check, configuring only one of them
    /// starts both happily — the other generates its own key — and the mismatch
    /// surfaces later as "wrong key or corrupt ciphertext" while opening a
    /// terminal or resolving a deploy's secrets. Checking at startup turns a
    /// mystery into a message.
    pub async fn verify_secret_key(&self, key: &crate::crypto::SecretKey) -> anyhow::Result<()> {
        /// What the verifier decrypts to. Its content does not matter; that it
        /// decrypts at all is the whole test.
        const VERIFIER_PLAINTEXT: &str = "nudo secret-store key check v1";

        let existing: Option<(String,)> =
            sqlx::query_as("SELECT verifier FROM secret_key_check WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;

        match existing {
            Some((sealed,)) => {
                let opened = key.open(&sealed).map_err(|_| {
                    anyhow::anyhow!(
                        "the configured secret key does not match the one this database was \
                         written with. Every stored secret — including your targets' SSH keys \
                         — was encrypted under the original key, so it must be supplied via \
                         NUDO_SECRET_KEY or NUDO_SECRET_KEY_FILE. If the control plane and the \
                         dashboard run as separate processes, both need the same key."
                    )
                })?;

                if opened != VERIFIER_PLAINTEXT {
                    anyhow::bail!(
                        "the secret-store key verifier in this database is not one nudo wrote"
                    );
                }
                Ok(())
            }
            None => {
                // First use: record what this key seals, so a later mismatch is
                // caught.
                sqlx::query(
                    "INSERT INTO secret_key_check (id, verifier, created_at) VALUES (1, ?1, ?2)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind(key.seal(VERIFIER_PLAINTEXT)?)
                .bind(now_string())
                .execute(&self.pool)
                .await?;
                Ok(())
            }
        }
    }
}
