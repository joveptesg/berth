-- Sections the admin keeps (Children, Psychology, Science fiction…): a flat list in their
-- order. A book may be in several. Aliases (one per line) match genres from file tags.
CREATE TABLE categories (
    id         TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL REFERENCES tenants (id),
    name       TEXT NOT NULL,
    aliases    TEXT NOT NULL DEFAULT '',
    position   INTEGER NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE UNIQUE INDEX categories_name_idx ON categories (tenant_id, name);

CREATE TABLE book_categories (
    tenant_id   TEXT NOT NULL REFERENCES tenants (id),
    book_id     TEXT NOT NULL REFERENCES books (id),
    category_id TEXT NOT NULL REFERENCES categories (id),
    PRIMARY KEY (tenant_id, book_id, category_id)
);
CREATE INDEX book_categories_category_idx ON book_categories (tenant_id, category_id);
