-- Lets the release check be turned off from the dashboard.
--
-- The config flag (--check-for-updates) sets whether the background loop runs
-- at all; this row is the operator's own choice, made after the process started,
-- and is checked on every tick. Off here wins over on in the config, so someone
-- who unticks the box does not have it come back on the next restart.

ALTER TABLE release_check ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
