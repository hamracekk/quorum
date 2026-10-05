-- Auto-accept (T-6): who decided a suggestion — `operator` or `policy` (the operator's
-- auto-accept policy). The policy's per-day cap counts its own decisions.
ALTER TABLE suggestions ADD COLUMN decided_by TEXT;
