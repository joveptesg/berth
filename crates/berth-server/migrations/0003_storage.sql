CREATE TABLE storage_profiles (
    id               TEXT PRIMARY KEY,
    tenant_id        TEXT NOT NULL REFERENCES tenants (id),
    name             TEXT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('local', 's3', 'r2', 'gdrive')),
    config_encrypted TEXT NOT NULL, -- AES-256-GCM, base64 (BLOB is not dialect-neutral)
    delivery_mode    TEXT NOT NULL CHECK (delivery_mode IN ('proxy', 'presigned', 'cdn')),
    created_at       BIGINT NOT NULL,
    deleted_at       BIGINT
);
CREATE UNIQUE INDEX storage_profiles_name_idx ON storage_profiles (tenant_id, name) WHERE deleted_at IS NULL;

-- Content-addressed blobs; deduplication only within a tenant.
CREATE TABLE blobs (
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    hash       TEXT NOT NULL, -- sha256, hex
    size       BIGINT NOT NULL,
    codec      TEXT,
    refcount   BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, hash)
);

-- Where the bytes are; a blob may live in several profiles during migration.
CREATE TABLE blob_locations (
    tenant_id  TEXT NOT NULL,
    hash       TEXT NOT NULL,
    storage_id TEXT NOT NULL REFERENCES storage_profiles (id),
    state      TEXT NOT NULL CHECK (state IN ('pending', 'active', 'deleting')),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, hash, storage_id),
    FOREIGN KEY (tenant_id, hash) REFERENCES blobs (tenant_id, hash)
);
CREATE INDEX blob_locations_storage_idx ON blob_locations (tenant_id, storage_id, state);
