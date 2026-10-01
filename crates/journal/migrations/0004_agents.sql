-- Agents' shared knowledge and messages (ADR 0003, AG-1). Findings are never edited by agents:
-- a newer finding supersedes an older one; only evaluation (AG-2) or the operator change status.
-- JSON columns hold typed values from trader_core::agents (instrument lists, hypothesis,
-- evidence, refs). AUTOINCREMENT keeps ids unique forever, so references stay valid.

CREATE TABLE findings (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    agent       TEXT NOT NULL,
    role        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (
        kind IN ('signal_edge', 'strategy_params', 'observation', 'news_impact', 'trader_quality')
    ),
    instruments TEXT NOT NULL,
    claim       TEXT NOT NULL,
    hypothesis  TEXT,
    evidence    TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'hypothesis' CHECK (
        status IN ('hypothesis', 'testing', 'confirmed', 'rejected', 'retired')
    ),
    verdict     TEXT,
    supersedes  INTEGER REFERENCES findings (id)
);

CREATE INDEX findings_agent ON findings (agent, id);
CREATE INDEX findings_status ON findings (status, id);
CREATE INDEX findings_supersedes ON findings (supersedes);

-- Full-text search over claims. Claims are immutable, so an insert trigger keeps it in sync.
CREATE VIRTUAL TABLE findings_fts USING fts5 (claim, content = 'findings', content_rowid = 'id');

CREATE TRIGGER findings_fts_insert AFTER INSERT ON findings BEGIN
    INSERT INTO findings_fts (rowid, claim) VALUES (new.id, new.claim);
END;

-- `thread` is the id of the thread's first message; sender/recipient are agent ids,
-- 'operator', or (recipient only) 'all'.
CREATE TABLE agent_messages (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        TEXT NOT NULL,
    thread    INTEGER NOT NULL,
    sender    TEXT NOT NULL,
    recipient TEXT NOT NULL,
    body      TEXT NOT NULL,
    refs      TEXT NOT NULL
);

CREATE INDEX agent_messages_thread ON agent_messages (thread, id);
CREATE INDEX agent_messages_recipient ON agent_messages (recipient, id);

CREATE TABLE agent_message_reads (
    message_id INTEGER NOT NULL REFERENCES agent_messages (id),
    reader     TEXT NOT NULL,
    read_at    TEXT NOT NULL,
    PRIMARY KEY (message_id, reader)
);

-- Runtime switches: one row per agent id, '*' for every agent.
CREATE TABLE agent_controls (
    agent      TEXT PRIMARY KEY,
    enabled    INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    reason     TEXT,
    updated_at TEXT NOT NULL
);
