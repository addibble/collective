-- Ledger rep decays with a 180-day half-life (collective_core::score).
-- Each entry stores its delta scaled up by how far past the ledger epoch
-- (2026-01-01) it was made, so a decayed balance is SUM(weight) scaled
-- back down to now, without rewriting history. Platform accounts (the
-- bounty pool) do not decay and keep using SUM(delta).
--
-- SQLite on D1 has no pow(), so entries made before this migration are
-- treated as made at the epoch. Only pre-launch development data exists
-- at this point.
ALTER TABLE ledger ADD COLUMN weight REAL NOT NULL DEFAULT 0;
UPDATE ledger SET weight = delta;
