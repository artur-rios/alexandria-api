-- UC-34/UC-35: local-login credentials (SRD §4.9) and the sessions a
-- successful local login creates (UC-34 postcondition: "a session must be
-- created to keep track of the login" — local mode authenticates
-- subsequent requests by session id rather than a bearer token).
CREATE TABLE IF NOT EXISTS local_login_credentials (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    email         TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sessions_expires_at ON sessions (expires_at);
