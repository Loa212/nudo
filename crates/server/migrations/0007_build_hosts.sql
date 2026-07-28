-- Build hosts — machines that build, reached over SSH.
--
-- Until now a build was always a subprocess of the control plane. That is a
-- reasonable default and stays the default, but it makes the control plane the
-- only place a build can run: a 1 vCPU box that is otherwise a dashboard, a
-- SQLite file and an SSH client has to compile whatever a service points at.
--
-- A build host is deliberately its own table rather than a flag on `targets`.
-- The two share reachability, an SSH user and a key, and nothing else — no
-- release root, no unit, nothing deployed here. Keeping them apart is what
-- makes "a build host is never deployed to, and a deploy target is never sent a
-- build" a property of the schema instead of a rule enforced by remembering.
--
-- Note on isolation: this table does not provide any. Two builds on one host
-- can see each other, and nudo does not try to change that — it is a property
-- of how the operator runs the host (one-shot container, ephemeral VM, fresh
-- instance per build). A build host is not a sandbox.

CREATE TABLE build_hosts (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    host              TEXT NOT NULL,
    port              INTEGER NOT NULL DEFAULT 22,
    user              TEXT NOT NULL,
    -- References secrets(id): as with targets, the SSH key lives in the secret
    -- store and never here.
    ssh_key_id        TEXT NOT NULL DEFAULT '',
    -- Where checkouts and build trees go. Each build gets a fresh directory
    -- underneath, removed when it finishes however it finishes.
    workspace_root    TEXT NOT NULL DEFAULT '/var/lib/nudo/builds',
    -- A build on a latency-critical box contends with whatever it is tuned for.
    -- Permitted, because an operator may have exactly one spare machine, but
    -- recorded so every surface can say so rather than accepting it silently.
    latency_critical  INTEGER NOT NULL DEFAULT 0,
    -- JSON object of label -> value, matching the targets table.
    labels            TEXT NOT NULL DEFAULT '{}',
    status            TEXT NOT NULL DEFAULT 'unknown',
    last_seen_at      TEXT,
    created_at        TEXT NOT NULL,

    -- Host-key pinning, identical in shape and meaning to the columns added to
    -- `targets` in 0006. A build host is handed repository credentials, so
    -- connecting to the wrong machine matters at least as much here as it does
    -- for a deploy target. Empty means "not pinned yet": the first successful
    -- connection records what the host presents, and every later one is
    -- verified against it.
    host_key                     TEXT NOT NULL DEFAULT '',
    host_key_fingerprint         TEXT NOT NULL DEFAULT '',
    host_key_pinned_at           TEXT,
    pending_host_key             TEXT NOT NULL DEFAULT '',
    pending_host_key_fingerprint TEXT NOT NULL DEFAULT '',
    pending_host_key_seen_at     TEXT
);

-- Which build host a service builds on, overriding the instance default.
--
-- Three states, and the difference is the point:
--   ''       -> unset; the instance default applies
--   'local'  -> the control plane, whatever the instance default is
--   <id>     -> that build host
--
-- Empty is the default, so every existing service keeps building exactly where
-- it builds today. There is no foreign key to build_hosts: a deleted build host
-- must leave the service pointing at a missing id and failing loudly at deploy
-- time, rather than being silently reset to "wherever the default is" — which
-- for a service that was deliberately built elsewhere would move the build
-- without anybody asking for it.
ALTER TABLE services ADD COLUMN git_build_host_id TEXT NOT NULL DEFAULT '';

-- The instance-wide default build host, in the single-row style the other
-- instance settings use. Empty — the initial state, and the state of every
-- instance that upgrades and configures nothing — means the control plane.
CREATE TABLE build_defaults (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    build_host_id   TEXT NOT NULL DEFAULT ''
);
