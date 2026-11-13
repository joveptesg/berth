-- Bookmarks of a user in books, made on any device. The device makes the id, so an offline
-- bookmark keeps it on every device. A deleted bookmark stays as a tombstone: other devices
-- learn of the deletion from it.
CREATE TABLE bookmarks (
    id          TEXT PRIMARY KEY, -- UUIDv7 made by the device
    tenant_id   TEXT NOT NULL REFERENCES tenants (id),
    user_id     TEXT NOT NULL REFERENCES users (id),
    book_id     TEXT NOT NULL REFERENCES books (id),
    position_ms BIGINT NOT NULL,
    note        TEXT NOT NULL DEFAULT '',
    created_at  BIGINT NOT NULL, -- corrected device time when it was made
    updated_at  BIGINT NOT NULL, -- corrected device time of the last change
    deleted     BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX bookmarks_user_idx ON bookmarks (tenant_id, user_id);
