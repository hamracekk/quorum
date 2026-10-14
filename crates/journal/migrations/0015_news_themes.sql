-- Theme readings of news (ADR 0008): which impact-map themes an item is about and which way
-- each moved, expanded into implied moves (`events`, JSON list of NewsEvent with `theme` set).
-- Kept apart from `news_extractions` so the per-item readings, their scoreboard record and the
-- ensemble stay exactly as they were. One row per item, classifier version and tier.

CREATE TABLE news_themes (
    url            TEXT NOT NULL REFERENCES news_items (url),
    prompt_version TEXT NOT NULL,
    tier           TEXT NOT NULL CHECK (tier IN ('local', 'remote')),
    model          TEXT NOT NULL,
    ts             TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('ok', 'rejected', 'failed')),
    relevant       INTEGER NOT NULL CHECK (relevant IN (0, 1)),
    events         TEXT NOT NULL,
    error          TEXT,
    input_tokens   INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens  INTEGER NOT NULL CHECK (output_tokens >= 0),
    PRIMARY KEY (url, prompt_version, tier)
);

CREATE INDEX news_themes_ts ON news_themes (ts);
