-- The language of a book (ISO 639-1, NULL when not known), and the languages a listener
-- sees (comma-separated codes, empty for all; books without a language stay visible).
ALTER TABLE books ADD COLUMN language TEXT;
ALTER TABLE access_policies ADD COLUMN languages TEXT NOT NULL DEFAULT '';
