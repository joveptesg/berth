-- What a book is about (a few sentences, NULL when not known) and the year the work was
-- first published.
ALTER TABLE books ADD COLUMN description TEXT;
ALTER TABLE books ADD COLUMN year BIGINT;
