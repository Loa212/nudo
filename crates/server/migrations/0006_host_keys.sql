-- Pinned SSH host keys.
--
-- Until now every host key was accepted, every time: a man-in-the-middle on a
-- re-registered or re-addressed target would be handed an authentication
-- attempt with the private key nudo holds for it, and nothing in the protocol
-- would notice. Host-key verification is the only thing that would.
--
-- The model is trust-on-first-use. The first successful connection records what
-- the host presented; every connection after that compares against it and fails
-- closed on a mismatch. That is deliberately weaker than confirming a
-- fingerprint out of band at registration time, and deliberately much stronger
-- than the nothing that was here before: it cannot tell you the first key was
-- the right one, but it will not let the key change behind your back.
--
-- Both the key and its fingerprint are stored. The fingerprint is what an
-- operator compares against `ssh-keyscan`, and the key itself is what makes a
-- change reviewable rather than merely reportable.

-- The pinned key, in OpenSSH one-line form ("ssh-ed25519 AAAA...").
ALTER TABLE targets ADD COLUMN host_key TEXT NOT NULL DEFAULT '';
-- Its SHA-256 fingerprint, in the "SHA256:..." form ssh-keygen prints. Derived
-- from host_key, stored so a listing does not have to parse every key.
ALTER TABLE targets ADD COLUMN host_key_fingerprint TEXT NOT NULL DEFAULT '';
-- When the pinned key was recorded or last accepted.
ALTER TABLE targets ADD COLUMN host_key_pinned_at TEXT;

-- A key seen on a connection that did not match the pinned one. Held here
-- rather than applied, so the change can be reviewed and accepted from the
-- dashboard or the CLI instead of by editing the database. Cleared when the
-- change is accepted or when a later connection presents the pinned key again.
ALTER TABLE targets ADD COLUMN pending_host_key TEXT NOT NULL DEFAULT '';
ALTER TABLE targets ADD COLUMN pending_host_key_fingerprint TEXT NOT NULL DEFAULT '';
ALTER TABLE targets ADD COLUMN pending_host_key_seen_at TEXT;

-- Existing targets have no recorded key, and an upgrade must not break a
-- working fleet: an empty host_key means "not pinned yet", so the first
-- connection after this migration records rather than refuses. That is exactly
-- the position a newly created target is in, so there is no special case for it
-- beyond leaving the column empty, which the DEFAULT above already does.
