-- A proposition may recur after an earlier episode ended. Existing rows and
-- their validity/provenance are retained; no missing history is invented.
DROP INDEX atomic_units_statement_hash_uq;
CREATE INDEX atomic_units_statement_hash_idx ON atomic_units(statement_hash);
