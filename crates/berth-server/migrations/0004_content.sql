CREATE TABLE libraries (
    id                 TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL REFERENCES tenants (id),
    name               TEXT NOT NULL,
    default_storage_id TEXT REFERENCES storage_profiles (id),
    visibility         TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private', 'link', 'public')),
    revision           BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    deleted_at         BIGINT
);
CREATE INDEX libraries_revision_idx ON libraries (tenant_id, revision);

CREATE TABLE collections (
    id         TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    library_id TEXT NOT NULL REFERENCES libraries (id),
    name       TEXT NOT NULL,
    storage_id TEXT REFERENCES storage_profiles (id), -- NULL = inherit from library
    revision   BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    deleted_at BIGINT
);
CREATE INDEX collections_revision_idx ON collections (tenant_id, revision);

CREATE TABLE books (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants (id),
    library_id    TEXT NOT NULL REFERENCES libraries (id),
    collection_id TEXT REFERENCES collections (id),
    storage_id    TEXT REFERENCES storage_profiles (id), -- NULL = cascade: collection, library, server default
    title         TEXT NOT NULL,
    series        TEXT,
    series_no     DOUBLE PRECISION,
    rating        INTEGER CHECK (rating IN (0, 6, 12, 16, 18)),
    duration_ms   BIGINT NOT NULL DEFAULT 0,
    revision      BIGINT NOT NULL,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL,
    deleted_at    BIGINT
);
CREATE INDEX books_revision_idx ON books (tenant_id, revision);
CREATE INDEX books_library_idx ON books (tenant_id, library_id);

CREATE TABLE book_people (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    book_id   TEXT NOT NULL REFERENCES books (id),
    role      TEXT NOT NULL CHECK (role IN ('author', 'narrator')),
    idx       INTEGER NOT NULL,
    person    TEXT NOT NULL,
    PRIMARY KEY (tenant_id, book_id, role, idx)
);
CREATE INDEX book_people_person_idx ON book_people (tenant_id, person);

CREATE TABLE book_tags (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    book_id   TEXT NOT NULL REFERENCES books (id),
    tag       TEXT NOT NULL,
    PRIMARY KEY (tenant_id, book_id, tag)
);
CREATE INDEX book_tags_tag_idx ON book_tags (tenant_id, tag);

-- Physical files of a book, placed on the book timeline.
CREATE TABLE book_files (
    tenant_id   TEXT NOT NULL,
    book_id     TEXT NOT NULL REFERENCES books (id),
    idx         INTEGER NOT NULL,
    blob_hash   TEXT NOT NULL,
    offset_ms   BIGINT NOT NULL,
    duration_ms BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, book_id, idx),
    FOREIGN KEY (tenant_id, blob_hash) REFERENCES blobs (tenant_id, hash)
);

-- Logical chapters on the same book timeline, independent of files.
CREATE TABLE chapters (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    book_id   TEXT NOT NULL REFERENCES books (id),
    idx       INTEGER NOT NULL,
    title     TEXT NOT NULL,
    start_ms  BIGINT NOT NULL,
    end_ms    BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, book_id, idx)
);
