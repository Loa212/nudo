-- A verifier proving the secret-store key matches the one this database's
-- ciphertexts were written under.
--
-- The control plane and the dashboard are separate processes sharing a database
-- and a key. Misconfigure one — set NUDO_SECRET_KEY for the server but not the
-- web tier — and both start happily: the second silently generates its own key
-- and everything works until something actually needs to decrypt, at which
-- point it fails as "wrong key or corrupt ciphertext" in the middle of opening a
-- terminal or running a deploy.
--
-- The first process to open a database seals a known plaintext here. Every later
-- process opens it, and refuses to start if it cannot. A misconfiguration is
-- then a startup error naming the problem, rather than a mystery at the worst
-- possible moment.

CREATE TABLE secret_key_check (
    -- A single row.
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    -- The known plaintext, sealed with the secret-store key.
    verifier          TEXT NOT NULL,
    created_at        TEXT NOT NULL
);
