-- Position suggestions (T-3): agents propose, the operator decides, the paper session applies.
-- `session_id` and `applied_after` let a resumed session replay an applied suggestion at the
-- same point (resuming is replay).
CREATE TABLE suggestions (
    id              INTEGER PRIMARY KEY,
    author          TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    instrument      TEXT NOT NULL,
    side            TEXT NOT NULL CHECK (side IN ('long', 'short', 'close')),
    size            REAL NOT NULL CHECK (size >= 0),
    stop_pct        REAL,
    take_profit_pct REAL,
    price           REAL,
    rationale       TEXT NOT NULL,
    evidence        TEXT NOT NULL DEFAULT '[]',
    confidence      REAL,
    status          TEXT NOT NULL
                    CHECK (status IN ('pending', 'accepted', 'rejected', 'expired', 'executed')),
    decided_at      TEXT,
    decision_reason TEXT,
    session_id      TEXT,
    applied_after   TEXT,
    executed_at     TEXT
);

CREATE INDEX suggestions_status ON suggestions (status, id);
CREATE INDEX suggestions_session ON suggestions (session_id, id);
