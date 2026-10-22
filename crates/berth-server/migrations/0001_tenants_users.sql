-- Dialect-neutral SQL (SQLite and Postgres): ids are UUIDv7 text, timestamps are epoch milliseconds,
-- no arrays (join tables), tenant_id in every tenant-owned table.

CREATE TABLE tenants (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants (id),
    name          TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'restricted')),
    password_hash TEXT, -- argon2id, web admin login only
    pin_hash      TEXT,
    created_at    BIGINT NOT NULL,
    deleted_at    BIGINT
);
CREATE INDEX users_tenant_idx ON users (tenant_id);

CREATE TABLE access_policies (
    tenant_id        TEXT NOT NULL REFERENCES tenants (id),
    user_id          TEXT NOT NULL REFERENCES users (id),
    max_rating       INTEGER CHECK (max_rating IN (0, 6, 12, 16, 18)), -- NULL = unrestricted
    unrated_behavior TEXT NOT NULL DEFAULT 'allowed' CHECK (unrated_behavior IN ('blocked', 'allowed')),
    policy_version   BIGINT NOT NULL DEFAULT 1, -- bumped on any policy or scope change; part of the sync cursor
    PRIMARY KEY (tenant_id, user_id)
);

CREATE TABLE policy_denied_tags (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    user_id   TEXT NOT NULL REFERENCES users (id),
    tag       TEXT NOT NULL,
    PRIMARY KEY (tenant_id, user_id, tag)
);

CREATE TABLE policy_scopes (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    user_id   TEXT NOT NULL REFERENCES users (id),
    kind      TEXT NOT NULL CHECK (kind IN ('library', 'collection', 'book')),
    target_id TEXT NOT NULL,
    effect    TEXT NOT NULL CHECK (effect IN ('allow', 'deny')),
    PRIMARY KEY (tenant_id, user_id, kind, target_id)
);

-- Per-tenant revision counter. Bump only with
-- `UPDATE revisions SET v = v + 1 WHERE tenant_id = ? RETURNING v` inside the write transaction:
-- the row lock keeps revision order equal to commit order (MAX + 1 skips rows in the sync delta).
CREATE TABLE revisions (
    tenant_id TEXT PRIMARY KEY REFERENCES tenants (id),
    v         BIGINT NOT NULL DEFAULT 0
);
