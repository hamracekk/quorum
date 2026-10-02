-- Reports to the operator (AG-3c): the orchestrator's weekly post-mortem. `facts` is JSON
-- computed by trader for the period; `body` is the agent's markdown reading of it.
CREATE TABLE agent_reports (
    id          INTEGER PRIMARY KEY,
    agent       TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    period_from TEXT NOT NULL,
    period_to   TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    facts       TEXT NOT NULL
);

CREATE INDEX agent_reports_agent_created ON agent_reports (agent, created_at);
