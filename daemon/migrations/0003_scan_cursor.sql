-- Contradiction-scanner cursor: an active atomic unit is eligible for a
-- contradiction scan while its marker is NULL; the scanner stamps it after
-- pairing it against candidates. Same pattern as units_extracted_at (0002).

ALTER TABLE atomic_units ADD COLUMN contradiction_scanned_at timestamptz;

CREATE INDEX atomic_units_scan_pending_idx
    ON atomic_units (id)
    WHERE contradiction_scanned_at IS NULL AND status = 'active';
