-- What the analysis found in each file of an upload: its duration once read, else a problem
-- code (`file_empty`, `file_not_audio`) and the reader's message. Both are NULL until checked.
ALTER TABLE upload_files ADD COLUMN duration_ms BIGINT;
ALTER TABLE upload_files ADD COLUMN problem TEXT;
ALTER TABLE upload_files ADD COLUMN problem_detail TEXT;

-- The journal of an upload the admin opens on its card: what happened, in order.
-- `event` is a code the admin translates; `count` and `detail` fill it in.
CREATE TABLE upload_events (
    id        TEXT PRIMARY KEY, -- UUIDv7: orders the events
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    upload_id TEXT NOT NULL REFERENCES uploads (id),
    at        BIGINT NOT NULL,
    event     TEXT NOT NULL,
    count     BIGINT,
    detail    TEXT
);
CREATE INDEX upload_events_upload_idx ON upload_events (upload_id, id);
