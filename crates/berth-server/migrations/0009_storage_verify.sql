-- Result of the last connection check of a storage profile: verified_at when it passed,
-- verify_error (a stable code such as storage_auth_failed) when it failed; both NULL until
-- checked or after the location or keys change. The tenant's default profile is the
-- 'default_storage_id' setting in scope 'tenant:<id>' (the end of the storage cascade).
ALTER TABLE storage_profiles ADD COLUMN verified_at BIGINT;
ALTER TABLE storage_profiles ADD COLUMN verify_error TEXT;
ALTER TABLE storage_profiles ADD COLUMN updated_at BIGINT;
