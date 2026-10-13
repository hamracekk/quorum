-- A market-wide risk-reducing control (robustness plan, step 5): `reduce_exposure` scales every
-- paper target down for a day; `stop` holds the scale (0–0.95). SQLite cannot change a CHECK
-- constraint in place, so the table is rebuilt with its rows.
CREATE TABLE position_controls_new (
    id            INTEGER PRIMARY KEY,
    author        TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    instrument    TEXT NOT NULL,
    action        TEXT NOT NULL CHECK (action IN ('tighten_stop', 'close', 'reduce_exposure')),
    stop          REAL,
    reason        TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'applied', 'refused')),
    note          TEXT,
    session_id    TEXT,
    applied_after TEXT,
    applied_at    TEXT
);

INSERT INTO position_controls_new
    SELECT id, author, created_at, instrument, action, stop, reason, status, note, session_id,
           applied_after, applied_at
    FROM position_controls;

DROP TABLE position_controls;
ALTER TABLE position_controls_new RENAME TO position_controls;

CREATE INDEX position_controls_status ON position_controls (status, id);
CREATE INDEX position_controls_session ON position_controls (session_id, id);
