-- Ingress — how traffic from the outside reaches a deployed service.
--
-- Until now nudo deployed a service and stopped there: getting a domain and a
-- certificate in front of it was entirely the operator's problem. This adds a
-- reverse proxy that nudo installs and configures, and a domain and port on the
-- service that says what to route where.
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
-- Both or neither: a domain with no port has nothing to route to, and a port
-- with no domain is documentation. Empty domain and zero port — what every
-- existing service gets — means this service is not routed, which is exactly
-- what is true of every service today.
ALTER TABLE services ADD COLUMN domain TEXT NOT NULL DEFAULT '';
ALTER TABLE services ADD COLUMN port INTEGER NOT NULL DEFAULT 0;

-- One domain, one service. Two services claiming the same hostname is not a
-- configuration nudo can render: whichever came second would silently never
-- receive traffic, and the operator would be debugging DNS for a problem that
-- is in the database.
--
-- A partial index so the empty string — every unrouted service — is exempt.
-- Without the WHERE clause the second service without a domain would collide
-- with the first.
CREATE UNIQUE INDEX services_domain_unique ON services(domain) WHERE domain != '';

-- One port per target. Two services on one host both listening on 8080 is a
-- collision that predates this feature, but until now nudo had no reason to
-- know about it. Once it is routing to ports it does, so it refuses rather than
-- generating a config whose second route can never work.
--
-- Scoped to the target because the same port on two different hosts is fine and
-- common. Zero is exempt for the same reason the empty domain is.
CREATE UNIQUE INDEX services_target_port_unique ON services(target_id, port) WHERE port != 0;
