-- LLM extraction of news (M5b). One row per item, prompt version and tier (`local` first
-- pass, `remote` escalation): the cache that keeps an item from being sent to the same tier
-- twice, the record of what each model read (`events`, JSON list of NewsEvent), and the token
-- spend the daily budget is checked against. Rejected and failed attempts are kept too.

CREATE TABLE news_extractions (
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

CREATE INDEX news_extractions_ts ON news_extractions (ts);
