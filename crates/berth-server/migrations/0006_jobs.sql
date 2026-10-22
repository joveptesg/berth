-- Background job queue. A worker claims a job with a conditional UPDATE that sets
-- locked_by/locked_until; an expired lease can be taken over by another worker.
CREATE TABLE jobs (
    id           TEXT PRIMARY KEY,
    tenant_id    TEXT NOT NULL REFERENCES tenants (id),
    kind         TEXT NOT NULL,
    payload      TEXT NOT NULL, -- JSON
    run_at       BIGINT NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    locked_by    TEXT,
    locked_until BIGINT,
    done_at      BIGINT,
    created_at   BIGINT NOT NULL
);
CREATE INDEX jobs_due_idx ON jobs (run_at) WHERE done_at IS NULL;
