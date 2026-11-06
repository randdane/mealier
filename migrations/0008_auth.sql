-- Step 8: authentication. Secrets are never stored: sessions and API tokens
-- keep the SHA-256 (hex) of the random value the client holds.
CREATE TABLE sessions (
    id_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at TEXT NOT NULL
);
CREATE INDEX sessions_user ON sessions (user_id);

CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_used_at TEXT
);
CREATE INDEX api_tokens_user ON api_tokens (user_id);

-- One row per login in flight; deleted when the callback arrives (single use).
CREATE TABLE oidc_logins (
    state_hash TEXT PRIMARY KEY,
    nonce TEXT NOT NULL,
    verifier TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

-- The break-glass admin (password in a file, no OIDC). At most one, and it can
-- never carry an OIDC binding, so an Authentik login cannot take it over.
ALTER TABLE users ADD COLUMN local_admin INTEGER NOT NULL DEFAULT 0 CHECK (local_admin IN (0, 1));
CREATE UNIQUE INDEX users_one_local_admin ON users (local_admin) WHERE local_admin = 1;
CREATE TRIGGER users_local_admin_no_oidc_insert BEFORE INSERT ON users
WHEN NEW.local_admin = 1 AND (NEW.oidc_issuer IS NOT NULL OR NEW.oidc_subject IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'the local admin cannot have an OIDC binding'); END;
CREATE TRIGGER users_local_admin_no_oidc_update BEFORE UPDATE ON users
WHEN NEW.local_admin = 1 AND (NEW.oidc_issuer IS NOT NULL OR NEW.oidc_subject IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'the local admin cannot have an OIDC binding'); END;
