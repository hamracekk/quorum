-- Orders (M3b): one row per order and run, holding its latest state. Every state change
-- arrives as an `order` event and upserts this row; the full history stays in the event log.
-- `order_id` is the engine's sequential id, unique within a run.

CREATE TABLE orders (
    run_id     TEXT NOT NULL REFERENCES runs (id),
    order_id   INTEGER NOT NULL CHECK (order_id > 0),
    instrument TEXT NOT NULL,
    side       TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    qty        REAL NOT NULL,
    status     TEXT NOT NULL CHECK (
        status IN ('new', 'accepted', 'partially_filled', 'filled', 'canceled', 'rejected')
    ),
    filled_qty REAL NOT NULL,
    avg_price  REAL NOT NULL,
    fees       REAL NOT NULL,
    reason     TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (run_id, order_id)
);

CREATE INDEX orders_run_status ON orders (run_id, status);
