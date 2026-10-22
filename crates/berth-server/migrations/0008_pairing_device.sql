-- The device created by exchanging a pairing token: the admin's /connect page shows
-- "connected" with the device name as soon as the phone pairs.
ALTER TABLE pairing_tokens ADD COLUMN device_id TEXT REFERENCES devices (id);
