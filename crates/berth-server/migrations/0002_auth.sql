CREATE TABLE devices (
    id                 TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL REFERENCES tenants (id),
    user_id            TEXT NOT NULL REFERENCES users (id),
    name               TEXT NOT NULL,
    platform           TEXT NOT NULL,
    refresh_hash       TEXT,
    refresh_expires_at BIGINT,
    rotated_from       TEXT, -- hash of the previous refresh token: rotation grace period and theft detection
    paired_at          BIGINT NOT NULL,
    last_seen_at       BIGINT,
    revoked_at         BIGINT
);
CREATE INDEX devices_tenant_user_idx ON devices (tenant_id, user_id);
CREATE UNIQUE INDEX devices_refresh_hash_idx ON devices (refresh_hash);

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
