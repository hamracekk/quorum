-- Journal schema v1 (ADR 0002). Never edit after merge: add a new migration instead.
-- Timestamps are RFC 3339 UTC with milliseconds ("2026-09-30T12:00:00.000Z"), so text
-- order equals time order. Instruments use their text form ("binance:BTCUSDT").

CREATE TABLE runs (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('backtest', 'replay', 'paper', 'live')),
    label      TEXT NOT NULL,
    started_at TEXT NOT NULL
);

-- A strategy changed its desired exposure.
CREATE TABLE signals (
    id         INTEGER PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs (id),
    ts         TEXT NOT NULL,
    instrument TEXT NOT NULL,
    strategy   TEXT NOT NULL,
    target     REAL NOT NULL
);

-- A risk-adjusted target queued for execution.
CREATE TABLE targets (
    id         INTEGER PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs (id),
    ts         TEXT NOT NULL,
    instrument TEXT NOT NULL,
    target     REAL NOT NULL
);

CREATE TABLE fills (
    id         INTEGER PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs (id),
    ts         TEXT NOT NULL,
    instrument TEXT NOT NULL,
    side       TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    qty        REAL NOT NULL,
    price      REAL NOT NULL,
    fee        REAL NOT NULL
);

CREATE TABLE risk_halts (
    id     INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs (id),
    ts     TEXT NOT NULL,
    equity REAL NOT NULL
);

CREATE INDEX signals_run_ts ON signals (run_id, ts);
CREATE INDEX targets_run_ts ON targets (run_id, ts);
CREATE INDEX fills_run_ts ON fills (run_id, ts);
CREATE INDEX risk_halts_run_ts ON risk_halts (run_id, ts);
