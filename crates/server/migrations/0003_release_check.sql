-- Where the release check records what it last saw.
--
-- One row. The manifest is stored whole so the dashboard's "What's new" can
-- render the changelog without another network call, and so an instance that
-- loses network access still shows the notes for the release it knows about.

CREATE TABLE release_check (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    -- The newest version this instance has been told about. Compared live
    -- against the running version rather than stored as a boolean, so the
    -- banner clears itself once the instance is upgraded.
    latest_version    TEXT NOT NULL,
    -- The whole manifest, as fetched.
    manifest          TEXT NOT NULL DEFAULT '{}',
    checked_at        TEXT NOT NULL
);
