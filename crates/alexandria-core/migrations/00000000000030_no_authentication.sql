-- Authentication is gone from the core: it only ever runs locally, for the
-- person at the machine. Every table it kept goes with it, and the stored
-- credentials with them. `auth_tokens` was already dropped by migration 14;
-- `IF EXISTS` keeps this safe against a catalog in any state. No other table
-- has a foreign key to these.
--
-- Libraries gain the two flags the owner sets on them, both off.
DROP TABLE IF EXISTS sessions;
DROP TABLE IF EXISTS local_login_credentials;
DROP TABLE IF EXISTS recovery_codes;
DROP TABLE IF EXISTS auth_tokens;
DROP INDEX IF EXISTS idx_sessions_expires_at;

ALTER TABLE libraries ADD COLUMN pinned   INTEGER NOT NULL DEFAULT 0 CHECK (pinned   IN (0, 1));
ALTER TABLE libraries ADD COLUMN favorite INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0, 1));
