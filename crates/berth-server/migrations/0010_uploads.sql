-- Book uploads from the admin or the API: one upload becomes one book.
-- uploading -> (all files stored) analyzing -> (ingest_pipeline job) ready -> (saved by the admin) saved;
-- failed when analysis gives up. `draft` is the proposed book (JSON) the admin edits before saving.
CREATE TABLE uploads (
    id         TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    library_id TEXT NOT NULL REFERENCES libraries (id),
    storage_id TEXT NOT NULL REFERENCES storage_profiles (id),
    name       TEXT NOT NULL, -- dropped folder or file name
    status     TEXT NOT NULL CHECK (status IN ('uploading', 'analyzing', 'ready', 'saved', 'failed')),
    draft      TEXT,
    error      TEXT,
    book_id    TEXT REFERENCES books (id),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX uploads_tenant_idx ON uploads (tenant_id, created_at);

-- Files of an upload in drop order; blob_hash is set once the bytes are stored.
CREATE TABLE upload_files (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    upload_id TEXT NOT NULL REFERENCES uploads (id),
    idx       INTEGER NOT NULL,
    name      TEXT NOT NULL, -- path inside the dropped folder
    size      BIGINT NOT NULL,
    kind      TEXT NOT NULL CHECK (kind IN ('audio', 'image')),
    blob_hash TEXT,
    stored    BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (upload_id, idx)
);

-- Cover image: a blob of the tenant (sha256), served by /v1/books/{id}/cover.
ALTER TABLE books ADD COLUMN cover_hash TEXT;
-- Title, people and series in lower case (Unicode-aware, written by the server): SQLite
-- LIKE folds only ASCII, so search matches against this column.
ALTER TABLE books ADD COLUMN search_text TEXT NOT NULL DEFAULT '';
