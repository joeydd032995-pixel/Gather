-- Review of contradictions the safety layer held back ("explained away").
--
-- Two new kinds of explicit user decision about a pair of claims:
--   contradiction_confirmed    — "this is a real conflict": the pair is reported
--                                and no automatic rule may explain it away again.
--   contradiction_not_conflict — "the explanation is right": the pair leaves the
--                                review list and counts as "not a conflict".
-- Only the check constraint changes. The reverse script is
-- migrations-down/0015_explained_away.down.sql.

ALTER TABLE semantic_user_decisions DROP CONSTRAINT semantic_user_decisions_kind_ck;
ALTER TABLE semantic_user_decisions ADD CONSTRAINT semantic_user_decisions_kind_ck CHECK
    (kind IN ('photo_not_duplicate', 'contradiction_confirmed', 'contradiction_not_conflict'));
