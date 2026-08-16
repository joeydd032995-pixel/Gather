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
    if winner_id == loser_id {
        return Err(ApiError::BadRequest(
            "cannot merge an entity into itself".to_string(),
        ));
    }
    let actor = actor.unwrap_or_else(|| "local-user".to_string());

    let mut tx = pool.begin().await?;

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
    .fetch_all(&mut *tx)
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
    let aliases_added = sqlx::query(
        r#"
        INSERT INTO entity_aliases (entity_id, alias)
        SELECT $1, alias FROM (
            SELECT $3::text AS alias
            UNION
            SELECT a.alias FROM entity_aliases a WHERE a.entity_id = $2
        ) src
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(winner_id)
    .bind(loser_id)
    .bind(&loser_name)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // The loser's own alias rows are now redundant; ON DELETE CASCADE would
    // only fire on a hard delete, and the merge is a soft one.
    sqlx::query("DELETE FROM entity_aliases WHERE entity_id = $1")
        .bind(loser_id)
        .execute(&mut *tx)
        .await?;

    // 2. Units whose subject was the loser now describe the winner.
    //
    // Clearing contradiction_scanned_at requeues them: §6.1 blocks candidate
    // pairs on shared subject entity, so these units could never be paired
    // against the winner's while the entities were separate. The scanner only
    // picks up units whose cursor is NULL (0003), so without this reset the
    // merge would repoint the rows but never surface the conflicts that the
    // split entity was suppressing — the whole point of resolving entities.
    let units_repointed = sqlx::query(
        "UPDATE atomic_units SET subject_entity_id = $1, contradiction_scanned_at = NULL \
         WHERE subject_entity_id = $2",
    )
    .bind(winner_id)
    .bind(loser_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // The winner's own units need re-pairing too: a unit is scanned once, so
    // those already stamped would otherwise never see the newly-arrived ones.
    let units_requeued = sqlx::query(
        "UPDATE atomic_units SET contradiction_scanned_at = NULL \
         WHERE subject_entity_id = $1 AND contradiction_scanned_at IS NOT NULL \
           AND status = 'active'",
    )
    .bind(winner_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 3a. An edge directly between the two entities becomes a self-loop once
    //     repointed, which `relationships_no_self_loop` rejects. Drop those.
    let self_loops = sqlx::query(
        r#"
        DELETE FROM relationships
        WHERE (source_entity_id = $1 AND target_entity_id = $2)
           OR (source_entity_id = $2 AND target_entity_id = $1)
        "#,
    )
    .bind(winner_id)
    .bind(loser_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 3b. Repointing can collide with an edge the winner already has, which
    //     `relationships_edge_uq` rejects. Drop the loser's duplicate rather
    //     than letting the whole transaction abort.
    let duplicates = sqlx::query(
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
        "#,
    )
    .bind(winner_id)
    .bind(loser_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 3c. Whatever survives can now be repointed safely.
    let sources =
        sqlx::query("UPDATE relationships SET source_entity_id = $1 WHERE source_entity_id = $2")
            .bind(winner_id)
            .bind(loser_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    let targets =
        sqlx::query("UPDATE relationships SET target_entity_id = $1 WHERE target_entity_id = $2")
            .bind(winner_id)
            .bind(loser_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    // 4. Soft-delete the loser. This also frees its name under the partial
    //    unique index entities_name_kind_uq, which only covers live rows.
    sqlx::query("UPDATE entities SET merged_into_entity_id = $1 WHERE id = $2")
        .bind(winner_id)
        .bind(loser_id)
        .execute(&mut *tx)
        .await?;

    // 4b. Flatten: anything previously merged INTO the loser now points at the
    //     winner directly. Rejecting already-merged operands (above) is not
    //     enough to prevent chains — a live entity that has itself absorbed
    //     others is a legal loser, so C→B followed by B→A would leave C→B→A.
    //     Keeping depth at exactly one means resolve_head is always one hop
    //     and never lands on an intermediate whose data has moved on.
    let descendants_flattened = sqlx::query(
        "UPDATE entities SET merged_into_entity_id = $1 WHERE merged_into_entity_id = $2",
    )
    .bind(winner_id)
    .bind(loser_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 5. Audit trail, same intent as contradiction_audit.
    sqlx::query(
        "INSERT INTO entity_merge_audit (winner_entity_id, loser_entity_id, action, actor, note) \
         VALUES ($1, $2, 'merge', $3, $4)",
    )
    .bind(winner_id)
    .bind(loser_id)
    .bind(&actor)
    .bind(&note)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    metrics::counter!("gather_entity_merges_total").increment(1);

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

/// Record that a reviewer rejected a suggested pair, so it stops being
/// suggested. Stored in the same audit table with action='dismiss'.
pub async fn dismiss_suggestion(
    pool: &PgPool,
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
    .execute(pool)
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
