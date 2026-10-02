-- Runs of internal agents (AG-3b): one row per run, updated when it ends. Tokens drive the
-- per-agent daily budget; the summary is the agent's closing text.
CREATE TABLE agent_runs (
    id            INTEGER PRIMARY KEY,
    agent         TEXT NOT NULL,
    task          TEXT NOT NULL,
    trigger       TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    status        TEXT NOT NULL CHECK (status IN ('running', 'done', 'stopped', 'failed')),
    steps         INTEGER NOT NULL DEFAULT 0 CHECK (steps >= 0),
    input_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    summary       TEXT,
    error         TEXT
);

CREATE INDEX agent_runs_agent_started ON agent_runs (agent, started_at);
