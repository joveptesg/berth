-- Groups of listeners (Adults, Children, Work…): each keeps an access rule of the same shape
-- as a listener's own. A listener in groups sees what any of their groups sees; one outside
-- groups keeps their own rule. (`groups` is a keyword in SQLite window frames.)
CREATE TABLE user_groups (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenants (id),
    name        TEXT NOT NULL,
    book_access TEXT NOT NULL DEFAULT 'all' CHECK (book_access IN ('all', 'selected')),
    languages   TEXT NOT NULL DEFAULT '', -- comma-separated codes; empty for all
    position    INTEGER NOT NULL,
    updated_at  BIGINT NOT NULL
);
CREATE INDEX user_groups_tenant_idx ON user_groups (tenant_id);

CREATE TABLE group_members (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    group_id  TEXT NOT NULL REFERENCES user_groups (id),
    user_id   TEXT NOT NULL REFERENCES users (id),
    PRIMARY KEY (tenant_id, group_id, user_id)
);
CREATE INDEX group_members_user_idx ON group_members (tenant_id, user_id);

CREATE TABLE group_books (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    group_id  TEXT NOT NULL REFERENCES user_groups (id),
    book_id   TEXT NOT NULL REFERENCES books (id),
    PRIMARY KEY (tenant_id, group_id, book_id)
);

CREATE TABLE group_categories (
    tenant_id   TEXT NOT NULL REFERENCES tenants (id),
    group_id    TEXT NOT NULL REFERENCES user_groups (id),
    category_id TEXT NOT NULL REFERENCES categories (id),
    PRIMARY KEY (tenant_id, group_id, category_id)
);

-- A child's date of birth, YYYY-MM-DD; NULL for an adult. `max_rating` keeps the ceiling it
-- gave last time, so a birthday that lifts it bumps the policy version.
ALTER TABLE access_policies ADD COLUMN birth_date TEXT;
