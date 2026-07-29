-- Ingress — how traffic from the outside reaches a deployed service.
--
-- Until now nudo deployed a service and stopped there: getting a domain and a
-- certificate in front of it was entirely the operator's problem. This adds a
-- reverse proxy that nudo installs and configures, and routes on the service
-- that say what to send where.
--
-- Ingress is a property of the target, not a service of its own. The tempting
-- alternative — Caddy as an ordinary nudo service — would have reused the
-- deploy engine, health checks and rollback for free. It was rejected because
-- the proxy is the thing every other service's traffic passes through: its
-- failure takes the whole host offline rather than one app, and modelling it as
-- a service would make it deletable, rollback-able and deployable through the
-- same paths as an ordinary workload. It also has no artifact source that fits
-- — Caddy comes from its own release page, not a git build or an upload.
--
-- Coolify solves this with Docker labels that Traefik reads off the socket. That
-- does not translate: nudo's unit of deployment is a systemd process on a host
-- it does not otherwise manage, so there is no ambient proxy watching anything
-- and no socket to attach a label to. The same feature therefore has to manage a
-- proxy rather than annotate a container.

-- Ingress settings live on the target. Nullable/defaulted throughout, so every
-- target that predates this has no ingress and behaves exactly as before.
--
-- 'none' is the default and means what it did before this existed: nudo does
-- not touch routing on this host.
--   'none'     -> no ingress
--   'managed'  -> nudo installs Caddy, writes its config and reloads it
--   'external' -> a proxy is already here; nudo renders the config it would
--                 write so it can be copied, but never touches the host
ALTER TABLE targets ADD COLUMN ingress_mode TEXT NOT NULL DEFAULT 'none';

-- Where Caddy's admin API listens, on loopback. The reload goes through this
-- rather than through a restart, because a restart drops every connection on
-- the host — including those of services that were not being changed.
ALTER TABLE targets ADD COLUMN ingress_admin_port INTEGER NOT NULL DEFAULT 2019;

-- The address Let's Encrypt sends expiry warnings to. Optional, because Caddy
-- issues certificates without one, but wanted: without it the first notice of
-- an expiring certificate is the outage it causes.
ALTER TABLE targets ADD COLUMN ingress_acme_email TEXT NOT NULL DEFAULT '';

-- Observed state, distinct from the settings above.
--   'pending'  -> configured, not yet installed or reached
--   'active'   -> installed, reachable, serving the config nudo last wrote
--   'degraded' -> installed but the last reload failed, or it is not answering;
--                 the routes it serves are the previous ones
ALTER TABLE targets ADD COLUMN ingress_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE targets ADD COLUMN ingress_version TEXT NOT NULL DEFAULT '';
ALTER TABLE targets ADD COLUMN ingress_last_reload_at TEXT;

-- Why the last reload failed, empty when it did not. Stored rather than only
-- logged: a degraded proxy is found by looking at the target, not by
-- remembering which deploy was the one that broke it.
ALTER TABLE targets ADD COLUMN ingress_last_error TEXT NOT NULL DEFAULT '';

-- Where a service is reachable from the outside.
--
-- A table rather than a `domain` and `port` on `services`, because one service
-- legitimately answers on several: an apex and its `www`, a public domain and
-- an internal one, or `/api` on a domain whose root is served by something
-- else. Coolify stores this as a comma-separated list of URLs on the
-- application row and parses host, path and port back out of each; the parts
-- are named here instead, which is the same model without the parsing.
--
-- A service with no rows here is not routed, which is what is true of every
-- service that predates this.
CREATE TABLE service_routes (
    id          TEXT PRIMARY KEY,
    service_id  TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,

    -- Denormalised from the service so the uniqueness constraint below can be
    -- scoped to a host without a join. Kept in step by the store, which is the
    -- only writer; a service cannot move between targets, so it cannot go
    -- stale.
    target_id   TEXT NOT NULL REFERENCES targets(id) ON DELETE CASCADE,

    domain      TEXT NOT NULL,
    -- '' means the whole domain. Otherwise a leading slash and no trailing one,
    -- normalised on the way in so '/api/', 'api' and '/api' are one route
    -- rather than three that collide confusingly.
    path        TEXT NOT NULL DEFAULT '',
    port        INTEGER NOT NULL,

    -- 'http' or 'h2c'. gRPC needs HTTP/2 end to end, and a proxy that
    -- downgrades to HTTP/1.1 silently breaks every call — so which one a
    -- service speaks is recorded rather than guessed.
    protocol    TEXT NOT NULL DEFAULT 'http',

    created_at  TEXT NOT NULL
);

-- One domain-and-path, one service. Two services claiming the same hostname and
-- prefix is not a configuration nudo can render: whichever came second would
-- silently never receive traffic, and the operator would be debugging DNS for a
-- problem that is in the database.
--
-- Scoped to include the path, so `example.com/api` and `example.com/admin` can
-- be different services — which is the point of having a path at all.
CREATE UNIQUE INDEX service_routes_domain_path_unique
    ON service_routes(domain, path);

-- One port per target. Two services on one host both listening on 8080 is a
-- collision that predates this feature, but until now nudo had no reason to
-- know about it. Once it is routing to ports it does, so it refuses rather than
-- generating a config whose second route can never work.
--
-- Scoped to the target because the same port on two different hosts is fine and
-- common. Not unique on (target_id, port) alone: one service can legitimately
-- have several routes to the same port — an apex and its `www` both reaching
-- 8080 — so the constraint is that no *other* service claims it, which the
-- store checks rather than the schema.
CREATE INDEX service_routes_target_port ON service_routes(target_id, port);

CREATE INDEX service_routes_service ON service_routes(service_id);
