-- Issue #102: e-mail confirmation state and the tokens both confirmation and
-- password reset present (FR-AU-13 … FR-AU-19).
--
-- Before this migration an account was created in a state that was neither
-- confirmed nor unconfirmed — the concept did not exist — and the owner's
-- password was unrecoverable, so a forgotten password meant a lost catalog.
--
-- Existing accounts get `email_confirmed_at = NULL`: unconfirmed, which is the
-- truthful state, since nothing has ever confirmed them. Nothing in the core
-- gates on it (FR-AU-13), so no install is locked out by this.
ALTER TABLE local_login_credentials ADD COLUMN email_confirmed_at TEXT;

-- One table for both purposes. They differ only in purpose, lifetime, and the
-- shape of what is generated; two tables would duplicate the expiry and
-- single-use logic that must not drift between them.
--
-- Only `token_hash` is stored, never the plaintext code or token — a database
-- read must not yield a working reset token, the same reason FR-AU-06 stores
-- no plaintext password. Lookups hash the presented value and match on that,
-- which is why the hash carries the unique index.
CREATE TABLE IF NOT EXISTS auth_tokens (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    purpose     TEXT NOT NULL,          -- 'email_confirmation' | 'password_reset'
    token_hash  TEXT NOT NULL,
    email       TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    consumed_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_auth_tokens_hash ON auth_tokens (token_hash);

-- Reads are "the most recent token of this purpose", for the resend interval
-- (FR-AU-15) and for telling an already-used code from an unknown one.
CREATE INDEX IF NOT EXISTS idx_auth_tokens_purpose_created
    ON auth_tokens (purpose, created_at);
