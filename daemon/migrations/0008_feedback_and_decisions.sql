-- Feedback loop + auto-act decision support (autonomous pipeline, Phase A).
--
-- The daemon moves from "suggest -> human confirms" to "act -> allow undo":
-- high-confidence work is auto-applied, the thin ambiguous middle is parked in
-- review_queue for OPTIONAL review, and the rare human correction is captured
-- as durable, reversible signal. These tables hold that signal. All local,
-- offline; nothing here reaches the network.

-- Append-only record of human corrections over any target (unit, entity,
-- merge, cluster). Same shape/intent as contradiction_audit (0001) and
-- entity_merge_audit (0004): who did what, when, with an optional payload.
-- action:
--   'confirm' — reviewer kept the item as-is (positive label).
--   'reject'  — reviewer vetoed it (negative label; caller also retracts it).
--   'edit'    — reviewer corrected it (payload in `corrected`).
CREATE TABLE unit_feedback (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    target_kind text NOT NULL,                          -- 'unit'|'entity'|'merge'|'cluster'
    target_id   uuid NOT NULL,
    action      text NOT NULL,                          -- 'confirm'|'reject'|'edit'
    actor       text NOT NULL DEFAULT 'local-user',
    corrected   jsonb,                                  -- edit payload, null otherwise
    note        text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT unit_feedback_action_ck
        CHECK (action IN ('confirm', 'reject', 'edit')),
    CONSTRAINT unit_feedback_kind_ck
        CHECK (target_kind IN ('unit', 'entity', 'merge', 'cluster'))
);

-- Precision is computed from these, sliced by target and action, so both
-- lookups are covered.
CREATE INDEX unit_feedback_target_idx ON unit_feedback (target_kind, target_id, created_at);
CREATE INDEX unit_feedback_action_idx ON unit_feedback (target_kind, action);

-- The "hold" tray: the ambiguous middle band the policy could not auto-decide.
-- Never blocks ingestion — a parked item still lives in the brain; parking only
-- flags it for optional attention. info_gain orders it (Phase C computes a real
-- value; until then it stays 0 and the tray is recency-ordered).
CREATE TABLE review_queue (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    target_kind text NOT NULL,                          -- 'unit'|'entity'|'merge'|'cluster'
    target_id   uuid NOT NULL,
    info_gain   real NOT NULL DEFAULT 0,
    reason      text NOT NULL,                          -- 'low-confidence','merge-band',...
    signals     jsonb NOT NULL DEFAULT '{}'::jsonb,     -- the scores behind the hold
    state       text NOT NULL DEFAULT 'open',           -- 'open'|'resolved'|'dismissed'
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT review_queue_state_ck
        CHECK (state IN ('open', 'resolved', 'dismissed')),
    CONSTRAINT review_queue_kind_ck
        CHECK (target_kind IN ('unit', 'entity', 'merge', 'cluster'))
);

-- Tray read order: highest info_gain first, then oldest.
CREATE INDEX review_queue_open_idx
    ON review_queue (info_gain DESC, created_at)
    WHERE state = 'open';

-- One open row per target: re-parking an item that is already open is a no-op
-- (ON CONFLICT DO NOTHING), so a re-scan cannot pile up duplicates.
CREATE UNIQUE INDEX review_queue_open_target_uq
    ON review_queue (target_kind, target_id)
    WHERE state = 'open';

-- Live decision thresholds. The feedback-driven tuner (Phase C) writes here so
-- the boundary adapts without a redeploy; env vars remain the default/floor
-- read when a key is absent.
CREATE TABLE decision_tuning (
    key        text PRIMARY KEY,
    value      double precision NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
