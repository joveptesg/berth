-- A closed section: its books only the owner and the listeners it is given to see.
ALTER TABLE categories ADD COLUMN closed INTEGER NOT NULL DEFAULT 0;

-- Sections given to a listener: with "selected" access they see these sections' books, and a
-- closed section opens to them.
CREATE TABLE user_categories (
    tenant_id   TEXT NOT NULL REFERENCES tenants (id),
    user_id     TEXT NOT NULL REFERENCES users (id),
    category_id TEXT NOT NULL REFERENCES categories (id),
    PRIMARY KEY (tenant_id, user_id, category_id)
);

-- Languages books can have, as the admin keeps them (ISO 639 code, name in the language).
CREATE TABLE languages (
    tenant_id TEXT NOT NULL REFERENCES tenants (id),
    code      TEXT NOT NULL,
    name      TEXT NOT NULL,
    position  INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, code)
);
INSERT INTO languages (tenant_id, code, name, position)
SELECT t.id, l.code, l.name, l.position FROM tenants t, (
    SELECT 'ru' AS code, 'Русский' AS name, 0 AS position
    UNION ALL SELECT 'en', 'English', 1
    UNION ALL SELECT 'de', 'Deutsch', 2
    UNION ALL SELECT 'fr', 'Français', 3
    UNION ALL SELECT 'es', 'Español', 4
    UNION ALL SELECT 'it', 'Italiano', 5
    UNION ALL SELECT 'zh', '中文', 6
    UNION ALL SELECT 'ja', '日本語', 7
) l;
-- A language a book already has stays in the list, named by its code until renamed.
INSERT OR IGNORE INTO languages (tenant_id, code, name, position)
SELECT DISTINCT tenant_id, language, language, 100 FROM books WHERE language IS NOT NULL;
