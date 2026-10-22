-- Admin sign-in to the web admin with the phone (mvp-spec §5): device code in reverse. The
-- browser shows a code (and its QR) and keeps a random claim in a cookie; the admin's paired
-- phone (scope `admin`) approves the code; only the browser holding the claim gets the session.
-- Only hashes are stored. Rows are removed by the hourly cleanup a day after they expire.
CREATE TABLE web_login_requests (
    code_hash   TEXT PRIMARY KEY,
    claim_hash  TEXT NOT NULL,
    tenant_id   TEXT NOT NULL REFERENCES tenants (id),
    browser     TEXT NOT NULL, -- "Chrome · Windows", shown on the phone before approving
    created_at  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL,
    approved_by TEXT REFERENCES devices (id),
    approved_at BIGINT,
    used_at     BIGINT
);
CREATE INDEX web_login_requests_expires_idx ON web_login_requests (expires_at);

-- Phones of administrators may approve web sign-ins: paired phones of owners and admins get the
-- `admin` scope (pairing now grants it too).
INSERT INTO device_scopes (tenant_id, device_id, scope)
SELECT d.tenant_id, d.id, 'admin'
FROM devices d
JOIN users u ON u.id = d.user_id
WHERE u.role IN ('owner', 'admin') AND d.revoked_at IS NULL
  -- Paired apps (any platform, the web build too) have listen; admin browsers do not.
  AND EXISTS (SELECT 1 FROM device_scopes s WHERE s.device_id = d.id AND s.scope = 'listen')
ON CONFLICT DO NOTHING;
