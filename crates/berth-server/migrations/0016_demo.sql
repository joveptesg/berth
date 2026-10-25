-- The demo library: public-domain LibriVox books the server downloads itself, one row a
-- book of the catalog the admin picked. `full` is the whole recording, else its first
-- chapters; `book_id` is the book while it is in the library.
CREATE TABLE demo_books (
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    slot       TEXT NOT NULL,
    full       INTEGER NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('queued', 'downloading', 'ready', 'failed')),
    book_id    TEXT,
    error      TEXT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, slot)
);
