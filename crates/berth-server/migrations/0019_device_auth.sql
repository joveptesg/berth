-- Sign-in of a desktop app with a device code (clients-spec §1, after RFC 8628). The app gets
-- a secret device code and a short user code; an administrator types or opens the user code in
-- the web admin (`/activate`) and allows it for a user; the app, polling with the device code,
-- then gets a new device of that user. Only hashes are stored. Rows are removed by the hourly
-- cleanup a day after they expire.
CREATE TABLE device_auth_requests (
    device_code_hash TEXT PRIMARY KEY,
    user_code_hash   TEXT NOT NULL UNIQUE,
    tenant_id        TEXT NOT NULL REFERENCES tenants (id),
    device_name      TEXT NOT NULL, -- shown on /activate before allowing
    platform         TEXT NOT NULL,
    created_at       BIGINT NOT NULL,
    expires_at       BIGINT NOT NULL,
    user_id          TEXT REFERENCES users (id), -- whose device it becomes, set on approval
    approved_by      TEXT REFERENCES users (id),
    approved_at      BIGINT,
    denied_at        BIGINT,
    used_at          BIGINT,
    device_id        TEXT REFERENCES devices (id)
);
CREATE INDEX device_auth_requests_expires_idx ON device_auth_requests (expires_at);
