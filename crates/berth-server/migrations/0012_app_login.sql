-- Apps sign in with a login and password too (a store reviewer has no QR code): a listener
-- may get a login; the password goes to users.password_hash, which the web admin accepts
-- only from the owner.
ALTER TABLE users ADD COLUMN login TEXT;
CREATE UNIQUE INDEX users_login_idx ON users (tenant_id, lower(login))
    WHERE login IS NOT NULL AND deleted_at IS NULL;

-- A user sees the whole library or only the books picked for them.
ALTER TABLE access_policies ADD COLUMN book_access TEXT NOT NULL DEFAULT 'all'
    CHECK (book_access IN ('all', 'selected'));

CREATE TABLE user_books (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    user_id   TEXT NOT NULL REFERENCES users (id),
    book_id   TEXT NOT NULL REFERENCES books (id),
    PRIMARY KEY (tenant_id, user_id, book_id)
);
