-- The release the operator chose to skip.
--
-- "Not now" and "not this one" are different answers, and only the second
-- should survive a page reload. Storing the version rather than a boolean is
-- what makes the next release come back on its own: the banner is suppressed
-- only while the newest release is exactly the one that was skipped.
ALTER TABLE release_check ADD COLUMN skipped_version TEXT NOT NULL DEFAULT '';
