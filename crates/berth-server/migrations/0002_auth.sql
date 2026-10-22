CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    tenant_id    TEXT NOT NULL REFERENCES tenants (id),
    user_id      TEXT NOT NULL REFERENCES users (id),
    name         TEXT NOT NULL,
    platform     TEXT NOT NULL,
    paired_at    BIGINT NOT NULL,
    last_seen_at BIGINT,
    revoked_at   BIGINT
);
CREATE INDEX devices_tenant_user_idx ON devices (tenant_id, user_id);

-- Refresh token chain of a device; only hashes are stored.
-- A successor is derived, not random: HMAC-SHA256(secret.key, device_id || token_hash), so a
-- concurrent refresh within the grace period gets the same new token without storing it.
-- Presenting a token:
--   rotated_at IS NULL                  -> rotate: insert the successor, set rotated_at
--                                          (conditional UPDATE; losing the race = grace path)
--   now - rotated_at <= grace (30-60 s) -> return the same successor
--   rotated_at older than grace         -> reuse = theft: delete the device's chain, session_expired
-- Rotated rows are kept until expires_at so reuse of any old token is detected.
CREATE TABLE refresh_tokens (
    token_hash TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    device_id  TEXT NOT NULL REFERENCES devices (id),
    issued_at  BIGINT NOT NULL,
    expires_at BIGINT NOT NULL, -- sliding window: issued_at + 60 days
    rotated_at BIGINT
);
CREATE INDEX refresh_tokens_device_idx ON refresh_tokens (tenant_id, device_id);
CREATE INDEX refresh_tokens_expires_idx ON refresh_tokens (expires_at);

CREATE TABLE device_scopes (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    device_id TEXT NOT NULL REFERENCES devices (id),
    scope     TEXT NOT NULL CHECK (scope IN ('listen', 'upload', 'admin', 'feed')),
    PRIMARY KEY (tenant_id, device_id, scope)
);

CREATE TABLE pairing_tokens (
    token_hash TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    user_id    TEXT REFERENCES users (id),
    flow_type  TEXT NOT NULL CHECK (flow_type IN ('qr', 'device_code', 'invite')),
    expires_at BIGINT NOT NULL,
    used_at    BIGINT
);
CREATE INDEX pairing_tokens_tenant_idx ON pairing_tokens (tenant_id, expires_at);
CREATE INDEX pairing_tokens_expires_idx ON pairing_tokens (expires_at);

-- Web admin session: the admin's browser is a device (platform 'web', scope 'admin');
-- the cookie holds a random token, only its hash is stored.
CREATE TABLE web_sessions (
    token_hash TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    device_id  TEXT NOT NULL REFERENCES devices (id),
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE INDEX web_sessions_device_idx ON web_sessions (tenant_id, device_id);
CREATE INDEX web_sessions_expires_idx ON web_sessions (expires_at);

-- Cleanup of dead tokens: a server task runs hourly (idempotent DELETEs, no lease needed) and removes
-- refresh_tokens, web_sessions and pairing_tokens past expires_at, pairing tokens used more than
-- a day ago, and web devices left without a session. Device revocation and refresh token theft
-- delete the device's chain at once. Rotated refresh tokens stay until their own expires_at
-- (at most 60 days): that is the window in which reuse of an old token is detected.
