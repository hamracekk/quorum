-- Audit of every tool call made by an AI agent through the MCP server (MCP track).
-- `arguments` is the call's JSON arguments; `summary` is a one-line result or the error.

CREATE TABLE agent_calls (
    id          INTEGER PRIMARY KEY,
    ts          TEXT NOT NULL,
    agent       TEXT NOT NULL,
    tool        TEXT NOT NULL,
    arguments   TEXT NOT NULL,
    ok          INTEGER NOT NULL CHECK (ok IN (0, 1)),
    summary     TEXT NOT NULL,
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0)
);

CREATE INDEX agent_calls_ts ON agent_calls (ts);
