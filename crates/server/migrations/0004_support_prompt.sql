-- The "support the project" prompt's state.
--
-- Two pieces: an instance-wide switch, and a per-user dismissal.
--
-- Per-user rather than in the browser's localStorage (which is what Coolify
-- does) so a dismissal is honoured on every device that person signs in from,
-- rather than resetting on each new browser.

CREATE TABLE support_prompt (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    -- Instance-wide off switch. On by default; one click in settings turns it
    -- off permanently.
    enabled           INTEGER NOT NULL DEFAULT 1
);

INSERT INTO support_prompt (id, enabled) VALUES (1, 1);

CREATE TABLE support_prompt_dismissals (
    user_id           TEXT PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- The prompt returns the following calendar month, so this is a timestamp
    -- rather than a boolean.
    dismissed_at      TEXT NOT NULL
);
