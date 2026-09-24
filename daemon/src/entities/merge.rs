//! Entity merge: fold a duplicate ("loser") into the entity that survives
//! ("winner"), so the knowledge graph converges on one node per real-world
//! thing.
//!
//! 0001_init laid out the whole design — `entity_aliases`,
//! `entities.merged_into_entity_id`, and the partial unique index
//! `entities_name_kind_uq (... WHERE merged_into_entity_id IS NULL)` — and
//! `extract::persist::resolve_or_create_entity` already resolves names against
//! entities UNION aliases. This module is the write half that was missing.

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::decide::MergeBasis;
use crate::error::ApiError;

/// Outcome of a merge, for the API response and for tests to assert against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MergeOutcome {
    pub winner_id: Uuid,
    pub loser_id: Uuid,
    pub winner_name: String,
    pub loser_name: String,
    /// Aliases now pointing at the winner that did not before (includes the
    /// loser's own name).
    pub aliases_added: u64,
    pub units_repointed: u64,
    /// Units whose contradiction-scan cursor was cleared so the newly-shared
    /// subject entity gets re-paired (both sides of the merge).
    pub units_requeued_for_scan: u64,
    pub relationships_repointed: u64,
    /// Edges dropped because repointing them would have violated
    /// `relationships_no_self_loop` or `relationships_edge_uq`.
    pub relationships_dropped: u64,
    /// Entities previously merged into the loser, repointed at the winner so
    /// the merge graph stays exactly one level deep.
    pub descendants_flattened: u64,
}

/// Merge `loser` into `winner` in one transaction.
///
/// Three constraints from 0001_init make the naive "just UPDATE the foreign
/// keys" version fail, so each is handled explicitly below:
///   * `entity_aliases_uq (entity_id, lower(alias))`
///   * `relationships_no_self_loop CHECK (source_entity_id <> target_entity_id)`
///   * `relationships_edge_uq (source, target, relation_type, coalesce(unit,...))`
pub async fn merge_entities(
    pool: &PgPool,
    winner_id: Uuid,
    loser_id: Uuid,
    note: Option<String>,
    actor: Option<String>,
) -> Result<MergeOutcome, ApiError> {
    let mut tx = pool.begin().await?;
    let outcome = merge_entities_in(&mut tx, winner_id, loser_id, note, actor, None).await?;
    tx.commit().await?;
    metrics::counter!("gather_entity_merges_total").increment(1);
    Ok(outcome)
}

/// [`merge_entities`] inside a caller's transaction, so a merge can commit
/// atomically with other writes (e.g. the review tray's tuning label). The
/// caller commits. `basis` is why the pipeline merged (the gate and its score;
/// None for a manual merge); it is kept so undoing the merge can teach the
/// tuner the right threshold.
///
/// Every change is journaled in the audit row's `undo` column, which is what
/// makes [`unmerge_entity_in`] exact.
pub async fn merge_entities_in(
    tx: &mut Transaction<'_, Postgres>,
    winner_id: Uuid,
    loser_id: Uuid,
    note: Option<String>,
    actor: Option<String>,
    basis: Option<MergeBasis>,
) -> Result<MergeOutcome, ApiError> {
    if winner_id == loser_id {
        return Err(ApiError::BadRequest(
            "cannot merge an entity into itself".to_string(),
        ));
    }
    let actor = actor.unwrap_or_else(|| "local-user".to_string());

    // Lock both rows in a stable order so two concurrent merges touching the
    // same pair cannot deadlock.
    let (first, second) = if winner_id < loser_id {
        (winner_id, loser_id)
    } else {
        (loser_id, winner_id)
    };
    let rows = sqlx::query(
        "SELECT id, name, merged_into_entity_id FROM entities \
         WHERE id IN ($1, $2) ORDER BY id FOR UPDATE",
    )
    .bind(first)
    .bind(second)
    .fetch_all(&mut **tx)
    .await?;
    if rows.len() != 2 {
        // Report whichever id is missing rather than a generic "not found".
        let found: Vec<Uuid> = rows.iter().map(|r| r.get("id")).collect();
        let missing = if found.contains(&winner_id) {
            loser_id
        } else {
            winner_id
        };
        return Err(ApiError::NotFound(format!("entity {missing}")));
    }

    let mut winner_name = String::new();
    let mut loser_name = String::new();
    for row in &rows {
        let id: Uuid = row.get("id");
        let name: String = row.get("name");
        let merged_into: Option<Uuid> = row.get("merged_into_entity_id");
        // Rejecting an already-merged operand is only half of keeping the
        // merge graph one level deep — a live entity that has itself absorbed
        // others is still a legal loser, so step 4b flattens its descendants.
        if let Some(head) = merged_into {
            return Err(ApiError::BadRequest(format!(
                "entity {id} is already merged into {head}"
            )));
        }
        if id == winner_id {
            winner_name = name;
        } else {
            loser_name = name;
        }
    }

    // 1. The loser's name becomes an alias of the winner — this is what makes
    //    a later re-sighting of that name resolve to the winner instead of
    //    creating a fresh node. Its existing aliases come along too.
    let loser_aliases: Vec<String> =
        sqlx::query_scalar("SELECT alias FROM entity_aliases WHERE entity_id = $1")
            .bind(loser_id)
            .fetch_all(&mut **tx)
            .await?;
    let aliases_added_to_winner: Vec<String> = sqlx::query_scalar(
        r#"
        INSERT INTO entity_aliases (entity_id, alias)
        SELECT $1, alias FROM (
            SELECT $3::text AS alias
            UNION
            SELECT a.alias FROM entity_aliases a WHERE a.entity_id = $2
        ) src
        ON CONFLICT DO NOTHING
        RETURNING alias
        "#,
    )
    .bind(winner_id)
    .bind(loser_id)
    .bind(&loser_name)
    .fetch_all(&mut **tx)
    .await?;
    let aliases_added = aliases_added_to_winner.len() as u64;

    // The loser's own alias rows are now redundant; ON DELETE CASCADE would
    // only fire on a hard delete, and the merge is a soft one.
    sqlx::query("DELETE FROM entity_aliases WHERE entity_id = $1")
        .bind(loser_id)
        .execute(&mut **tx)
        .await?;

    // 2. Units whose subject was the loser now describe the winner.
    //
    // Clearing contradiction_scanned_at requeues them: §6.1 blocks candidate
    // pairs on shared subject entity, so these units could never be paired
    // against the winner's while the entities were separate. The scanner only
    // picks up units whose cursor is NULL (0003), so without this reset the
    // merge would repoint the rows but never surface the conflicts that the
    // split entity was suppressing — the whole point of resolving entities.
    let moved_units: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE atomic_units SET subject_entity_id = $1, contradiction_scanned_at = NULL \
         WHERE subject_entity_id = $2 RETURNING id",
    )
    .bind(winner_id)
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    let units_repointed = moved_units.len() as u64;

    // The winner's own units need re-pairing too: a unit is scanned once, so
    // those already stamped would otherwise never see the newly-arrived ones.
    let units_requeued = sqlx::query(
        "UPDATE atomic_units SET contradiction_scanned_at = NULL \
         WHERE subject_entity_id = $1 AND contradiction_scanned_at IS NOT NULL \
           AND status = 'active'",
    )
    .bind(winner_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 3a. An edge directly between the two entities becomes a self-loop once
    //     repointed, which `relationships_no_self_loop` rejects. Drop those.
    // Deleted edges are journaled whole so an undo can put them back.
    let mut deleted_edges: Vec<serde_json::Value> = sqlx::query_scalar(
        r#"
        DELETE FROM relationships r
        WHERE (source_entity_id = $1 AND target_entity_id = $2)
           OR (source_entity_id = $2 AND target_entity_id = $1)
        RETURNING to_jsonb(r)
        "#,
    )
    .bind(winner_id)
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    let self_loops = deleted_edges.len() as u64;

    // 3b. Repointing can collide with an edge the winner already has, which
    //     `relationships_edge_uq` rejects. Drop the loser's duplicate rather
    //     than letting the whole transaction abort.
    let duplicate_edges: Vec<serde_json::Value> = sqlx::query_scalar(
        r#"
        DELETE FROM relationships loser_edge
        WHERE (loser_edge.source_entity_id = $2 OR loser_edge.target_entity_id = $2)
          AND EXISTS (
              SELECT 1 FROM relationships kept
              WHERE kept.relation_type = loser_edge.relation_type
                AND coalesce(kept.atomic_unit_id, '00000000-0000-0000-0000-000000000000'::uuid)
                  = coalesce(loser_edge.atomic_unit_id, '00000000-0000-0000-0000-000000000000'::uuid)
                AND kept.id <> loser_edge.id
                AND kept.source_entity_id
                  = CASE WHEN loser_edge.source_entity_id = $2 THEN $1
                         ELSE loser_edge.source_entity_id END
                AND kept.target_entity_id
                  = CASE WHEN loser_edge.target_entity_id = $2 THEN $1
                         ELSE loser_edge.target_entity_id END
          )
        RETURNING to_jsonb(loser_edge)
        "#,
    )
    .bind(winner_id)
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    let duplicates = duplicate_edges.len() as u64;
    deleted_edges.extend(duplicate_edges);

    // 3c. Whatever survives can now be repointed safely.
    let source_edges: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE relationships SET source_entity_id = $1 WHERE source_entity_id = $2 RETURNING id",
    )
    .bind(winner_id)
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    let target_edges: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE relationships SET target_entity_id = $1 WHERE target_entity_id = $2 RETURNING id",
    )
    .bind(winner_id)
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    let sources = source_edges.len() as u64;
    let targets = target_edges.len() as u64;

    // 4. Soft-delete the loser. This also frees its name under the partial
    //    unique index entities_name_kind_uq, which only covers live rows.
    sqlx::query("UPDATE entities SET merged_into_entity_id = $1 WHERE id = $2")
        .bind(winner_id)
        .bind(loser_id)
        .execute(&mut **tx)
        .await?;

    // 4b. Flatten: anything previously merged INTO the loser now points at the
    //     winner directly. Rejecting already-merged operands (above) is not
    //     enough to prevent chains — a live entity that has itself absorbed
    //     others is a legal loser, so C→B followed by B→A would leave C→B→A.
    //     Keeping depth at exactly one means resolve_head is always one hop
    //     and never lands on an intermediate whose data has moved on.
    let descendants: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE entities SET merged_into_entity_id = $1 WHERE merged_into_entity_id = $2 \
         RETURNING id",
    )
    .bind(winner_id)
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    let descendants_flattened = descendants.len() as u64;

    // 5. Audit trail, same intent as contradiction_audit.
    //    The journal records exactly what moved, so unmerge can reverse it.
    let journal = MergeJournal {
        units: moved_units,
        source_edges,
        target_edges,
        deleted_edges,
        aliases_added_to_winner,
        loser_aliases,
        descendants,
        gate: basis.map(|b| b.gate.as_str().to_string()),
    };
    sqlx::query(
        "INSERT INTO entity_merge_audit \
           (winner_entity_id, loser_entity_id, action, actor, note, undo, score) \
         VALUES ($1, $2, 'merge', $3, $4, $5, $6)",
    )
    .bind(winner_id)
    .bind(loser_id)
    .bind(&actor)
    .bind(&note)
    .bind(serde_json::to_value(&journal).map_err(anyhow::Error::from)?)
    .bind(basis.map(|b| b.score))
    .execute(&mut **tx)
    .await?;

    Ok(MergeOutcome {
        winner_id,
        loser_id,
        winner_name,
        loser_name,
        aliases_added,
        units_repointed,
        units_requeued_for_scan: units_repointed + units_requeued,
        relationships_repointed: sources + targets,
        relationships_dropped: self_loops + duplicates,
        descendants_flattened,
    })
}

/// Everything a merge changed, stored with its audit row so the merge can be
/// reversed exactly.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct MergeJournal {
    /// Units whose subject moved from the loser to the winner.
    units: Vec<Uuid>,
    /// Edges repointed on their source / target side.
    source_edges: Vec<Uuid>,
    target_edges: Vec<Uuid>,
    /// Edges deleted (self-loops and duplicates), as whole rows.
    deleted_edges: Vec<serde_json::Value>,
    /// Aliases the merge added to the winner (the loser's name among them).
    aliases_added_to_winner: Vec<String>,
    /// The loser's own aliases, deleted from it by the merge.
    loser_aliases: Vec<String>,
    /// Entities previously merged into the loser, flattened onto the winner.
    descendants: Vec<Uuid>,
    /// The gate that admitted an automatic or tray merge ("single" or
    /// "agreement"), so an undo tunes the threshold that caused it.
    #[serde(default)]
    gate: Option<String>,
}

/// What an unmerge restored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UnmergeOutcome {
    pub winner_id: Uuid,
    pub loser_id: Uuid,
    pub units_restored: u64,
    pub relationships_restored: u64,
    pub aliases_restored: u64,
    pub descendants_restored: u64,
    /// Open contradictions withdrawn because they only existed while the two
    /// entities shared a subject.
    pub contradictions_withdrawn: u64,
}

/// Undo the merge that folded `loser_id` away. See [`unmerge_entity_in`].
pub async fn unmerge_entity(
    pool: &PgPool,
    loser_id: Uuid,
    note: Option<String>,
    actor: Option<String>,
) -> Result<UnmergeOutcome, ApiError> {
    let mut tx = pool.begin().await?;
    let outcome = unmerge_entity_in(&mut tx, loser_id, note, actor).await?;
    tx.commit().await?;
    metrics::counter!("gather_entity_unmerges_total").increment(1);
    Ok(outcome)
}

/// Reverse the most recent live merge of `loser_id` from its journal: the
/// entity comes back with its name, aliases, units, edges and descendants.
///
/// The pair is then dismissed so the clustering worker never re-merges it,
/// and — when the merge carried a similarity score — the undo is recorded as
/// a negative merge label, which is what lets the tuner raise the auto-merge
/// bar after wrong merges. Refused when the winner has itself been merged
/// away since (undo that merge first), or for merges made before journaling.
pub async fn unmerge_entity_in(
    tx: &mut Transaction<'_, Postgres>,
    loser_id: Uuid,
    note: Option<String>,
    actor: Option<String>,
) -> Result<UnmergeOutcome, ApiError> {
    let actor = actor.unwrap_or_else(|| "local-user".to_string());
    let merge = sqlx::query(
        "SELECT id, winner_entity_id, undo, score, created_at FROM entity_merge_audit \
         WHERE loser_entity_id = $1 AND action = 'merge' AND undone_at IS NULL \
         ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(loser_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("no reversible merge for entity {loser_id}")))?;
    let merge_id: Uuid = merge.get("id");
    let winner_id: Uuid = merge.get("winner_entity_id");
    let score: Option<f32> = merge.get("score");
    let merged_at: chrono::DateTime<chrono::Utc> = merge.get("created_at");
    let journal: MergeJournal = match merge.get::<Option<serde_json::Value>, _>("undo") {
        Some(v) => serde_json::from_value(v).map_err(anyhow::Error::from)?,
        None => {
            return Err(ApiError::BadRequest(format!(
                "entity {loser_id} was merged before merges were journaled; it cannot be \
                 split automatically"
            )))
        }
    };

    // Lock both rows (stable order, like merge) and check the merge is still
    // the live state: the loser points at this winner, the winner is live.
    let (first, second) = if winner_id < loser_id {
        (winner_id, loser_id)
    } else {
        (loser_id, winner_id)
    };
    let rows = sqlx::query(
        "SELECT id, name, kind::text AS kind, merged_into_entity_id FROM entities \
         WHERE id IN ($1, $2) ORDER BY id FOR UPDATE",
    )
    .bind(first)
    .bind(second)
    .fetch_all(&mut **tx)
    .await?;
    let row = |id: Uuid| {
        rows.iter()
            .find(|r| r.get::<Uuid, _>("id") == id)
            .ok_or_else(|| ApiError::NotFound(format!("entity {id}")))
    };
    let winner = row(winner_id)?;
    let loser = row(loser_id)?;
    if let Some(head) = winner.get::<Option<Uuid>, _>("merged_into_entity_id") {
        return Err(ApiError::BadRequest(format!(
            "entity {winner_id} has since been merged into {head}; undo that merge first"
        )));
    }
    if loser.get::<Option<Uuid>, _>("merged_into_entity_id") != Some(winner_id) {
        return Err(ApiError::BadRequest(format!(
            "entity {loser_id} is no longer merged into {winner_id}"
        )));
    }
    // Merges into the same survivor interleave (a later merge can repoint or
    // delete what an earlier one moved), so they unwind newest-first. Undoing
    // an older one first could silently lose edges the later journal owns.
    let newer: Option<Uuid> = sqlx::query_scalar(
        "SELECT loser_entity_id FROM entity_merge_audit \
         WHERE action = 'merge' AND undone_at IS NULL AND winner_entity_id = $1 \
           AND created_at > $2 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(winner_id)
    .bind(merged_at)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(later) = newer {
        return Err(ApiError::BadRequest(format!(
            "entity {later} was merged into {winner_id} more recently; undo that merge first"
        )));
    }
    let loser_name: String = loser.get("name");
    let loser_kind: String = loser.get("kind");
    // The loser's name must still be free among live entities of its kind
    // (entities_name_kind_uq), or bringing it back would collide.
    let clash: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM entities WHERE lower(name) = lower($1) AND kind::text = $2 \
         AND merged_into_entity_id IS NULL AND id <> $3 LIMIT 1",
    )
    .bind(&loser_name)
    .bind(&loser_kind)
    .bind(loser_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(other) = clash {
        return Err(ApiError::BadRequest(format!(
            "another live entity ({other}) now has the name '{loser_name}'; merge or rename it first"
        )));
    }

    // 1. Bring the loser (and whatever had been merged into it) back.
    sqlx::query("UPDATE entities SET merged_into_entity_id = NULL WHERE id = $1")
        .bind(loser_id)
        .execute(&mut **tx)
        .await?;
    let descendants_restored = sqlx::query(
        "UPDATE entities SET merged_into_entity_id = $2 \
         WHERE id = ANY($1) AND merged_into_entity_id = $3",
    )
    .bind(&journal.descendants)
    .bind(loser_id)
    .bind(winner_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 2. Aliases: take back what the merge gave the winner, return the loser's own.
    sqlx::query("DELETE FROM entity_aliases WHERE entity_id = $1 AND alias = ANY($2)")
        .bind(winner_id)
        .bind(&journal.aliases_added_to_winner)
        .execute(&mut **tx)
        .await?;
    let aliases_restored = sqlx::query(
        "INSERT INTO entity_aliases (entity_id, alias) SELECT $1, a FROM UNNEST($2::text[]) a \
         ON CONFLICT DO NOTHING",
    )
    .bind(loser_id)
    .bind(&journal.loser_aliases)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 3. Units go back to describing the loser, and are rescanned apart.
    let units_restored = sqlx::query(
        "UPDATE atomic_units SET subject_entity_id = $2, contradiction_scanned_at = NULL \
         WHERE id = ANY($1) AND subject_entity_id = $3",
    )
    .bind(&journal.units)
    .bind(loser_id)
    .bind(winner_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 3b. Open contradictions the scanner found between the two sides while
    //     they shared a subject are withdrawn: subject-blocking produced them,
    //     and the units no longer share one. Rescanning (cursors were just
    //     cleared) re-finds any that still hold on other evidence. Only
    //     untouched ('open') ones detected after the merge are withdrawn.
    let contradictions_withdrawn = sqlx::query(
        "DELETE FROM contradictions c WHERE c.status = 'open' AND c.detected_at >= $3 AND ( \
           (c.unit_a_id = ANY($1) AND c.unit_b_id IN \
              (SELECT id FROM atomic_units WHERE subject_entity_id = $2)) \
        OR (c.unit_b_id = ANY($1) AND c.unit_a_id IN \
              (SELECT id FROM atomic_units WHERE subject_entity_id = $2)))",
    )
    .bind(&journal.units)
    .bind(winner_id)
    .bind(merged_at)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 4. Edges: repoint the moved ones back, re-insert the deleted ones.
    let sources = sqlx::query(
        "UPDATE relationships SET source_entity_id = $2 \
         WHERE id = ANY($1) AND source_entity_id = $3",
    )
    .bind(&journal.source_edges)
    .bind(loser_id)
    .bind(winner_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let targets = sqlx::query(
        "UPDATE relationships SET target_entity_id = $2 \
         WHERE id = ANY($1) AND target_entity_id = $3",
    )
    .bind(&journal.target_edges)
    .bind(loser_id)
    .bind(winner_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let reinserted = sqlx::query(
        "INSERT INTO relationships \
         SELECT * FROM jsonb_populate_recordset(NULL::relationships, $1::jsonb) \
         ON CONFLICT DO NOTHING",
    )
    .bind(serde_json::Value::Array(journal.deleted_edges))
    .execute(&mut **tx)
    .await?
    .rows_affected();

    // 5. The loser leaves any auto-merge group it was counted in.
    let groups: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM cluster_members WHERE member_kind = 'entity' AND member_id = $1 \
         RETURNING cluster_id",
    )
    .bind(loser_id)
    .fetch_all(&mut **tx)
    .await?;
    if !groups.is_empty() {
        sqlx::query(
            "UPDATE clusters c SET size = \
               (SELECT count(*) FROM cluster_members m WHERE m.cluster_id = c.id), \
               updated_at = now() WHERE c.id = ANY($1)",
        )
        .bind(&groups)
        .execute(&mut **tx)
        .await?;
        sqlx::query("DELETE FROM clusters WHERE id = ANY($1) AND size < 2")
            .bind(&groups)
            .execute(&mut **tx)
            .await?;
    }

    // 6. Audit: close the merge, record the unmerge, and dismiss the pair so
    //    the clustering worker never merges it again.
    sqlx::query("UPDATE entity_merge_audit SET undone_at = now() WHERE id = $1")
        .bind(merge_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO entity_merge_audit (winner_entity_id, loser_entity_id, action, actor, note) \
         VALUES ($1, $2, 'unmerge', $3, $4), ($1, $2, 'dismiss', $3, 'merge undone')",
    )
    .bind(winner_id)
    .bind(loser_id)
    .bind(&actor)
    .bind(&note)
    .execute(&mut **tx)
    .await?;

    // 7. A wrong merge the pipeline made is the tuner's negative label.
    if let Some(score) = score {
        sqlx::query(
            "INSERT INTO unit_feedback \
               (target_kind, target_id, action, actor, corrected, note, score) \
             VALUES ('merge', $1, 'reject', $2, $3, $4, $5)",
        )
        .bind(crate::cluster::pair_key(winner_id, loser_id))
        .bind(&actor)
        .bind(serde_json::json!({
            "a": winner_id,
            "b": loser_id,
            "undone_merge": merge_id,
            "gate": journal.gate,
        }))
        .bind(&note)
        .bind(score)
        .execute(&mut **tx)
        .await?;
    }

    Ok(UnmergeOutcome {
        winner_id,
        loser_id,
        units_restored,
        relationships_restored: sources + targets + reinserted,
        aliases_restored,
        descendants_restored,
        contradictions_withdrawn,
    })
}

/// Record that a reviewer rejected a suggested pair, so it stops being
/// suggested. Stored in the same audit table with action='dismiss'.
pub async fn dismiss_suggestion(
    pool: &PgPool,
    a_id: Uuid,
    b_id: Uuid,
    note: Option<String>,
    actor: Option<String>,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    dismiss_suggestion_in(&mut tx, a_id, b_id, note, actor).await?;
    tx.commit().await?;
    Ok(())
}

/// [`dismiss_suggestion`] inside a caller's transaction. The caller commits.
pub async fn dismiss_suggestion_in(
    tx: &mut Transaction<'_, Postgres>,
    a_id: Uuid,
    b_id: Uuid,
    note: Option<String>,
    actor: Option<String>,
) -> Result<(), ApiError> {
    if a_id == b_id {
        return Err(ApiError::BadRequest(
            "cannot dismiss a pair of one entity".to_string(),
        ));
    }
    let actor = actor.unwrap_or_else(|| "local-user".to_string());
    sqlx::query(
        "INSERT INTO entity_merge_audit (winner_entity_id, loser_entity_id, action, actor, note) \
         VALUES ($1, $2, 'dismiss', $3, $4)",
    )
    .bind(a_id)
    .bind(b_id)
    .bind(&actor)
    .bind(&note)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Follow `merged_into_entity_id` to the surviving entity. Readers use this so
/// a link to a merged-away id keeps working instead of returning an empty
/// graph. Bounded to avoid spinning on unexpected cycles; merges reject
/// already-merged operands, so chains should not form in the first place.
pub async fn resolve_head(pool: &PgPool, id: Uuid) -> Result<Uuid, ApiError> {
    let mut current = id;
    for _ in 0..8 {
        let next: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
                .bind(current)
                .fetch_optional(pool)
                .await?;
        match next {
            None => return Ok(current), // no such row; let the caller 404
            Some(None) => return Ok(current),
            Some(Some(head)) => current = head,
        }
    }
    Ok(current)
}

/// Transaction-scoped variant of [`resolve_head`].
pub async fn resolve_head_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Uuid, ApiError> {
    let mut current = id;
    for _ in 0..8 {
        let next: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
                .bind(current)
                .fetch_optional(&mut **tx)
                .await?;
        match next {
            None => return Ok(current),
            Some(None) => return Ok(current),
            Some(Some(head)) => current = head,
        }
    }
    Ok(current)
}
