-- Risk-reducing actions on one open paper position (T-5): a watcher agent tightens a stop or
-- closes the position. Applied by the paper session without approval (they only reduce risk)
-- and replayed at the same point when it resumes.
CREATE TABLE position_controls (
    id            INTEGER PRIMARY KEY,
    author        TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    instrument    TEXT NOT NULL,
    action        TEXT NOT NULL CHECK (action IN ('tighten_stop', 'close')),
    stop          REAL,
    reason        TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'applied', 'refused')),
    note          TEXT,
    session_id    TEXT,
    applied_after TEXT,
    applied_at    TEXT
);

CREATE INDEX position_controls_status ON position_controls (status, id);
CREATE INDEX position_controls_session ON position_controls (session_id, id);
