-- Why a strategy chose a target (M8): the ensemble's contributing sources, their weights and
-- the sizing, as JSON. NULL for strategies that do not explain themselves.
ALTER TABLE signals ADD COLUMN reason TEXT;
