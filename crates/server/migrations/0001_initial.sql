-- Initial schema for the nudo control plane.
--
-- SQLite, single file, applied automatically on startup. Timestamps are stored
-- as ISO-8601 UTC text, which sqlx maps to chrono::DateTime<Utc> and which
-- sorts lexicographically — so ORDER BY on a timestamp column is chronological
-- without any conversion.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Targets — the machines we deploy to
-- ---------------------------------------------------------------------------

CREATE TABLE targets (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    host              TEXT NOT NULL,
    port              INTEGER NOT NULL DEFAULT 22,
    user              TEXT NOT NULL,
    -- References secrets(id): the SSH key lives in the secret store, never here.
    ssh_key_id        TEXT NOT NULL DEFAULT '',
    latency_critical  INTEGER NOT NULL DEFAULT 0,
    -- JSON object of label -> value, used by the label_selector filter.
    labels            TEXT NOT NULL DEFAULT '{}',
    status            TEXT NOT NULL DEFAULT 'unknown',
    last_seen_at      TEXT,
    created_at        TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Sources — GitHub App installations and deploy keys
-- ---------------------------------------------------------------------------

CREATE TABLE sources (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    kind              TEXT NOT NULL,
    api_url           TEXT NOT NULL DEFAULT 'https://api.github.com',
    html_url          TEXT NOT NULL DEFAULT 'https://github.com',
    app_id            INTEGER,
    app_slug          TEXT NOT NULL DEFAULT '',
    client_id         TEXT NOT NULL DEFAULT '',
    installation_id   INTEGER,
    account_login     TEXT NOT NULL DEFAULT '',
    organization      TEXT NOT NULL DEFAULT '',
    -- Sealed with the secret-store key. Never returned over the API.
    private_key_enc   TEXT,
    webhook_secret_enc TEXT,
    client_secret_enc TEXT,
    -- Public half of a deploy key, so the operator can paste it into GitHub.
    deploy_public_key TEXT NOT NULL DEFAULT '',
    installed         INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL
);

-- The webhook receiver looks a source up by the app id GitHub sends in
-- X-GitHub-Hook-Installation-Target-Id, on every delivery.
CREATE INDEX idx_sources_app_id ON sources (app_id);

-- Pending GitHub App manifest flows. Keyed by sha256(state) rather than the
-- raw state, so a database read cannot replay the flow, and consumed exactly
-- once when GitHub redirects back.
CREATE TABLE github_setup_states (
    state_hash        TEXT PRIMARY KEY,
    source_id         TEXT NOT NULL REFERENCES sources (id) ON DELETE CASCADE,
    action            TEXT NOT NULL,
    expires_at        TEXT NOT NULL,
    created_at        TEXT NOT NULL
);

-- Cached installation access tokens. GitHub issues these with a one-hour life;
-- caching them keyed on expiry avoids minting a JWT and a token on every clone,
-- API call and branch listing.
CREATE TABLE github_installation_tokens (
    source_id         TEXT PRIMARY KEY REFERENCES sources (id) ON DELETE CASCADE,
    token_enc         TEXT NOT NULL,
    expires_at        TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Services — one systemd unit on one target
-- ---------------------------------------------------------------------------

CREATE TABLE services (
    id                TEXT PRIMARY KEY,
    target_id         TEXT NOT NULL REFERENCES targets (id) ON DELETE CASCADE,
    name              TEXT NOT NULL,

    -- Artifact source: exactly one of url / git / direct upload.
    artifact_kind     TEXT NOT NULL DEFAULT 'direct_upload',
    artifact_url      TEXT NOT NULL DEFAULT '',
    git_source_id     TEXT REFERENCES sources (id) ON DELETE SET NULL,
    git_repo          TEXT NOT NULL DEFAULT '',
    git_branch        TEXT NOT NULL DEFAULT '',
    git_build_command TEXT NOT NULL DEFAULT '',
    git_artifact_path TEXT NOT NULL DEFAULT '',
    git_auto_deploy   INTEGER NOT NULL DEFAULT 0,

    -- Systemd unit definition.
    unit_name         TEXT NOT NULL DEFAULT '',
    unit_description  TEXT NOT NULL DEFAULT '',
    exec_args         TEXT NOT NULL DEFAULT '',
    working_directory TEXT NOT NULL DEFAULT '',
    unit_user         TEXT NOT NULL DEFAULT '',
    unit_group        TEXT NOT NULL DEFAULT '',
    restart           TEXT NOT NULL DEFAULT 'always',
    restart_sec       INTEGER NOT NULL DEFAULT 5,
    after_units       TEXT NOT NULL DEFAULT '[]',
    cpu_affinity      TEXT NOT NULL DEFAULT '',
    nice              TEXT NOT NULL DEFAULT '',
    io_scheduling_class TEXT NOT NULL DEFAULT '',
    extra_directives  TEXT NOT NULL DEFAULT '{}',

    -- Health check: exactly one of http / command / systemd_active.
    health_kind       TEXT NOT NULL DEFAULT 'systemd_active',
    health_http_url   TEXT NOT NULL DEFAULT '',
    health_command    TEXT NOT NULL DEFAULT '',
    health_timeout_seconds INTEGER NOT NULL DEFAULT 10,
    health_retries    INTEGER NOT NULL DEFAULT 3,
    health_initial_delay_seconds INTEGER NOT NULL DEFAULT 2,

    release_root      TEXT NOT NULL,
    keep_releases     INTEGER NOT NULL DEFAULT 5,
    secret_ids        TEXT NOT NULL DEFAULT '[]',
    env               TEXT NOT NULL DEFAULT '{}',
    current_release_id TEXT NOT NULL DEFAULT '',
    created_at        TEXT NOT NULL,

    -- A target cannot have two services of the same name, since they would
    -- collide on the unit name and the release root.
    UNIQUE (target_id, name)
);

CREATE INDEX idx_services_target ON services (target_id);
-- The push webhook resolves a delivery to services by repo + branch.
CREATE INDEX idx_services_git ON services (git_source_id, git_repo, git_branch);

-- ---------------------------------------------------------------------------
-- Releases and deployments
-- ---------------------------------------------------------------------------

CREATE TABLE releases (
    id                TEXT PRIMARY KEY,
    service_id        TEXT NOT NULL REFERENCES services (id) ON DELETE CASCADE,
    git_sha           TEXT NOT NULL DEFAULT '',
    git_ref           TEXT NOT NULL DEFAULT '',
    artifact_digest   TEXT NOT NULL DEFAULT '',
    artifact_bytes    INTEGER NOT NULL DEFAULT 0,
    path              TEXT NOT NULL,
    -- Cleared by the retention sweep once the release directory is removed from
    -- the target, so rollback never offers a release that is no longer there.
    pruned            INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL
);

CREATE INDEX idx_releases_service ON releases (service_id, created_at DESC);

CREATE TABLE deployments (
    id                TEXT PRIMARY KEY,
    service_id        TEXT NOT NULL REFERENCES services (id) ON DELETE CASCADE,
    release_id        TEXT NOT NULL DEFAULT '',
    status            TEXT NOT NULL,
    actor_kind        TEXT NOT NULL DEFAULT 'system',
    actor_id          TEXT NOT NULL DEFAULT '',
    actor_label       TEXT NOT NULL DEFAULT '',
    previous_release_id TEXT NOT NULL DEFAULT '',
    error             TEXT NOT NULL DEFAULT '',
    -- Set when a client asks to cancel; the engine checks it between steps and
    -- unwinds rather than being killed mid-write.
    cancel_requested  INTEGER NOT NULL DEFAULT 0,
    git_sha           TEXT NOT NULL DEFAULT '',
    git_ref           TEXT NOT NULL DEFAULT '',
    trigger           TEXT NOT NULL DEFAULT 'manual',
    started_at        TEXT NOT NULL,
    finished_at       TEXT
);

CREATE INDEX idx_deployments_service ON deployments (service_id, started_at DESC);
CREATE INDEX idx_deployments_status ON deployments (status);

-- Build and deploy output, persisted so a deployment view opened after the fact
-- shows what happened rather than an empty pane.
CREATE TABLE deployment_logs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    deployment_id     TEXT NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    at                TEXT NOT NULL,
    stderr            INTEGER NOT NULL DEFAULT 0,
    line              TEXT NOT NULL
);

CREATE INDEX idx_deployment_logs ON deployment_logs (deployment_id, id);

-- ---------------------------------------------------------------------------
-- Secrets
-- ---------------------------------------------------------------------------

CREATE TABLE secrets (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    -- Sealed with the secret-store key. Write-only over the API.
    value_enc         TEXT NOT NULL,
    -- sha256 of the plaintext, so a client can detect drift without reading it.
    digest            TEXT NOT NULL,
    scope_target_id   TEXT REFERENCES targets (id) ON DELETE CASCADE,
    scope_service_id  TEXT REFERENCES services (id) ON DELETE CASCADE,
    updated_at        TEXT NOT NULL,
    created_at        TEXT NOT NULL
);

-- A name is unique within its scope, so Put can upsert deterministically.
-- COALESCE because SQLite treats NULLs as distinct in a unique index, which
-- would otherwise allow duplicate global secrets.
CREATE UNIQUE INDEX idx_secrets_scope ON secrets (
    name,
    COALESCE(scope_target_id, ''),
    COALESCE(scope_service_id, '')
);

-- ---------------------------------------------------------------------------
-- Auth
-- ---------------------------------------------------------------------------

CREATE TABLE users (
    id                TEXT PRIMARY KEY,
    email             TEXT NOT NULL UNIQUE,
    password_hash     TEXT NOT NULL,
    display_name      TEXT NOT NULL DEFAULT '',
    created_at        TEXT NOT NULL
);

CREATE TABLE sessions (
    -- sha256 of the cookie value: a database read cannot impersonate a user.
    id                TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    csrf_token        TEXT NOT NULL,
    expires_at        TEXT NOT NULL,
    created_at        TEXT NOT NULL
);

CREATE INDEX idx_sessions_user ON sessions (user_id);

CREATE TABLE api_tokens (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    -- sha256 of the token. The plaintext is shown once, at creation.
    token_hash        TEXT NOT NULL UNIQUE,
    -- Comma-separated scopes: 'read' and/or 'write'.
    scopes            TEXT NOT NULL DEFAULT 'read',
    created_by        TEXT NOT NULL DEFAULT '',
    last_used_at      TEXT,
    revoked_at        TEXT,
    expires_at        TEXT,
    created_at        TEXT NOT NULL
);

-- Short-lived, single-use terminal grants. The websocket endpoint accepts only
-- one of these; it never accepts a host or credentials from the client.
CREATE TABLE terminal_sessions (
    id                TEXT PRIMARY KEY,
    -- sha256 of the token handed to the client.
    token_hash        TEXT NOT NULL UNIQUE,
    target_id         TEXT NOT NULL REFERENCES targets (id) ON DELETE CASCADE,
    initial_command   TEXT NOT NULL DEFAULT '',
    cols              INTEGER NOT NULL DEFAULT 80,
    rows              INTEGER NOT NULL DEFAULT 24,
    -- Set the moment the token is redeemed, so a leaked token cannot be reused.
    consumed_at       TEXT,
    expires_at        TEXT NOT NULL,
    created_at        TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- Audit
-- ---------------------------------------------------------------------------

CREATE TABLE audit_entries (
    id                TEXT PRIMARY KEY,
    at                TEXT NOT NULL,
    actor_kind        TEXT NOT NULL,
    actor_id          TEXT NOT NULL DEFAULT '',
    actor_label       TEXT NOT NULL DEFAULT '',
    action            TEXT NOT NULL,
    subject_id        TEXT NOT NULL DEFAULT '',
    dry_run           INTEGER NOT NULL DEFAULT 0,
    summary           TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_audit_at ON audit_entries (at DESC);
CREATE INDEX idx_audit_subject ON audit_entries (subject_id, at DESC);
CREATE INDEX idx_audit_actor ON audit_entries (actor_kind, at DESC);

-- ---------------------------------------------------------------------------
-- Idempotency
-- ---------------------------------------------------------------------------

-- Records the result of a mutating call carrying an idempotency key, so a retry
-- after a dropped connection returns the original outcome instead of deploying
-- twice.
CREATE TABLE idempotency_keys (
    key               TEXT PRIMARY KEY,
    action            TEXT NOT NULL,
    -- The id of whatever the original call produced.
    result_id         TEXT NOT NULL DEFAULT '',
    created_at        TEXT NOT NULL
);
