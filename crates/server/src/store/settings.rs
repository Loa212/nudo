use super::*;

impl Store {
    /// Whether the release check is turned on for this instance.
    ///
    /// Defaults to on, and only ever consulted when the config flag already
    /// allows checking — so unticking the box in the dashboard stops it, and no
    /// setting here can turn it on against the operator's config.
    pub async fn release_check_enabled(&self) -> anyhow::Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT enabled FROM release_check WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(enabled,)| enabled != 0).unwrap_or(true))
    }

    /// Turns the release check on or off from the dashboard.
    pub async fn set_release_check_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO release_check (id, latest_version, manifest, checked_at, enabled)
             VALUES (1, '', '{}', ?2, ?1)
             ON CONFLICT (id) DO UPDATE SET enabled = ?1",
        )
        .bind(enabled as i64)
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// When the release check last ran, for the settings page.
    pub async fn release_checked_at(
        &self,
    ) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT checked_at FROM release_check WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(at,)| from_db_time(&at)))
    }

    /// Whether the "support the project" prompt is shown on this instance.
    ///
    /// Defaults to on. One click in settings turns it off for good.
    pub async fn support_prompt_enabled(&self) -> anyhow::Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT enabled FROM support_prompt WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(enabled,)| enabled != 0).unwrap_or(true))
    }

    /// Turns the support prompt on or off for the whole instance.
    pub async fn set_support_prompt_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO support_prompt (id, enabled) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET enabled = ?1",
        )
        .bind(enabled as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether the operator has opted this instance into self-upgrading.
    ///
    /// Defaults to off, unlike the other toggles: this one authorises the
    /// process to replace its own binaries, and the config flag
    /// (`--allow-self-upgrade`) has to be on as well.
    pub async fn self_upgrade_enabled(&self) -> anyhow::Result<bool> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT enabled FROM self_upgrade_settings WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(enabled,)| enabled != 0).unwrap_or(false))
    }

    /// Turns the self-upgrade opt-in on or off for the whole instance.
    pub async fn set_self_upgrade_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO self_upgrade_settings (id, enabled) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET enabled = ?1",
        )
        .bind(enabled as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// When this user last dismissed the support prompt.
    pub async fn support_prompt_dismissed_at(
        &self,
        user_id: &str,
    ) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT dismissed_at FROM support_prompt_dismissals WHERE user_id = ?1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(at,)| from_db_time(&at)))
    }

    /// Records that this user dismissed the support prompt.
    ///
    /// Stored per user rather than in the browser, so it is honoured on every
    /// device they sign in from.
    pub async fn dismiss_support_prompt(&self, user_id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO support_prompt_dismissals (user_id, dismissed_at)
             VALUES (?1, ?2)
             ON CONFLICT (user_id) DO UPDATE SET dismissed_at = ?2",
        )
        .bind(user_id)
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How many deployments have succeeded, so the support prompt can wait until
    /// someone has actually got value from the tool.
    pub async fn count_successful_deployments(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM deployments WHERE status = 'succeeded'")
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    /// The newest version the release check has seen, or empty when it has not
    /// run yet.
    pub async fn recorded_latest_version(&self) -> anyhow::Result<String> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT latest_version FROM release_check WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(version,)| version).unwrap_or_default())
    }

    /// The manifest the release check last fetched, for rendering the changelog
    /// without a network call.
    pub async fn recorded_manifest(&self) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT manifest FROM release_check WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(manifest,)| manifest))
    }

    /// Records what the release check found.
    pub async fn record_latest_version(&self, version: &str, manifest: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO release_check (id, latest_version, manifest, checked_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET
                 latest_version = ?1, manifest = ?2, checked_at = ?3",
        )
        .bind(version)
        .bind(manifest)
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The release the operator chose to skip, empty when none was.
    pub async fn skipped_version(&self) -> anyhow::Result<String> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT skipped_version FROM release_check WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(version,)| version).unwrap_or_default())
    }

    /// Records a release as skipped.
    ///
    /// The version rather than a flag: skipping 0.4.0 must not hide 0.5.0 when
    /// it lands, and "I have decided about this one" is the thing actually
    /// being remembered.
    pub async fn skip_version(&self, version: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO release_check (id, latest_version, manifest, checked_at, skipped_version)
             VALUES (1, '', '{}', ?2, ?1)
             ON CONFLICT (id) DO UPDATE SET skipped_version = ?1",
        )
        .bind(version)
        .bind(now_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
