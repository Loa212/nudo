-- The dashboard's self-upgrade opt-in. A single-row table like support_prompt:
-- one instance, one switch. Default off — the config flag alone must never be
-- enough to let an instance replace its own binaries.
CREATE TABLE self_upgrade_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0
);
