-- UC-43/UC-44: the recovery codes that replace e-mail password reset
-- (FR-AU-13 … FR-AU-19).
--
-- Only `code_hash` is stored, never the code itself. A recovery code
-- overrides the password, so a database read that yielded a working code
-- would be exactly as bad as one that yielded a working password — the same
-- reasoning that keeps FR-AU-06 from storing a plaintext one. Lookups hash
-- the presented value and match on that, which is why the hash carries the
-- unique index.
--
-- There is no expiry column. A code is written on paper and used years later
-- or never; an expiry would silently turn the owner's only way back in into
-- nothing, on a schedule they did not choose. `consumed_at` is the whole
-- lifecycle: NULL means usable, set means spent.
CREATE TABLE IF NOT EXISTS recovery_codes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    code_hash   TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    consumed_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_recovery_codes_hash
    ON recovery_codes (code_hash);
