-- AG-2: evaluation of findings. `confidence` is the author's stated probability (0–1) that the
-- claim will be confirmed, scored as a Brier score. Every evaluation or operator decision is
-- kept in `finding_evaluations`; `findings.verdict` holds the latest one.

ALTER TABLE findings ADD COLUMN confidence REAL CHECK (confidence IS NULL OR (confidence >= 0 AND confidence <= 1));

CREATE TABLE finding_evaluations (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    finding_id INTEGER NOT NULL REFERENCES findings (id),
    ts         TEXT NOT NULL,
    by_whom    TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (
        status IN ('hypothesis', 'testing', 'confirmed', 'rejected', 'retired')
    ),
    verdict    TEXT NOT NULL
);

CREATE INDEX finding_evaluations_finding ON finding_evaluations (finding_id, id);
