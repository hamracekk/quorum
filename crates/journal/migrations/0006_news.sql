-- News items (M5a): untrusted headlines from public feeds, stored as data. `url` is the
-- canonical link and identifies an item; `title_key` (lower-case words of the title) lets the
-- store drop the same story from another feed within 48 hours. `instruments` is a JSON list
-- of configured instruments the item mentions.

CREATE TABLE news_items (
    url          TEXT PRIMARY KEY,
    source       TEXT NOT NULL,
    title        TEXT NOT NULL,
    title_key    TEXT NOT NULL,
    summary      TEXT NOT NULL,
    published_at TEXT NOT NULL,
    fetched_at   TEXT NOT NULL,
    instruments  TEXT NOT NULL
);

CREATE INDEX news_items_published ON news_items (published_at);
CREATE INDEX news_items_title_key ON news_items (title_key, published_at);
CREATE INDEX news_items_source ON news_items (source, published_at);

-- Full-text search over titles and summaries. Items are never edited, so an insert trigger
-- keeps the index in sync.
CREATE VIRTUAL TABLE news_fts USING fts5 (title, summary, content = 'news_items', content_rowid = 'rowid');

CREATE TRIGGER news_fts_insert AFTER INSERT ON news_items BEGIN
    INSERT INTO news_fts (rowid, title, summary) VALUES (new.rowid, new.title, new.summary);
END;
