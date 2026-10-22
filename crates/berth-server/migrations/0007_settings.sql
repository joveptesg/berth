-- Settings editable from the UI. scope = 'server' or 'tenant:<id>'.
-- Plain values are JSON; secrets go to value_encrypted (AES-256-GCM, base64).
CREATE TABLE settings (
    scope           TEXT NOT NULL,
    key             TEXT NOT NULL,
    value           TEXT,
    value_encrypted TEXT,
    updated_at      BIGINT NOT NULL,
    PRIMARY KEY (scope, key)
);
