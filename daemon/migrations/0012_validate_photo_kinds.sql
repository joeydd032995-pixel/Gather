-- Validate the widened kind checks added NOT VALID in 0011. VALIDATE takes only
-- a SHARE UPDATE EXCLUSIVE lock, so reads and writes continue meanwhile.
ALTER TABLE clusters VALIDATE CONSTRAINT clusters_kind_ck;
ALTER TABLE cluster_members VALIDATE CONSTRAINT cluster_members_kind_ck;
