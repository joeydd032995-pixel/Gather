//! Entity resolution: aliases, merges, and duplicate suggestions.

pub mod merge;
pub mod similarity;

use std::collections::HashSet;

use pgvector::Vector;
use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::ollama::OllamaClient;

pub use merge::{dismiss_suggestion, merge_entities, resolve_head, MergeOutcome};
pub use similarity::DEFAULT_THRESHOLD;

/// How many live entities the text pass will consider. Scoring is pairwise, so
/// this bounds the work at `cap²/2` comparisons — trivial at personal scale
/// (a few thousand entities), and a hard ceiling if a bulk import ever creates
/// far more.
const TEXT_PASS_ENTITY_CAP: i64 = 2_000;

/// Max cosine distance for the embedding pass; the complement of the score
/// threshold, since `<=>` returns distance and callers think in similarity.
const EMBEDDING_MAX_DISTANCE: f64 = 0.35;

#[derive(Debug, Clone, Serialize)]
pub struct EntityRef {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeSuggestion {
    pub a: EntityRef,
    pub b: EntityRef,
    pub score: f32,
    /// "rule:name-similarity" or "embedding:cosine" — which signal fired.
    pub method: &'static str,
}

/// Candidate duplicate pairs, highest score first.
///
/// Two passes, mirroring how `scan` blocks candidates: a text pass that always
/// runs, and an embedding pass that contributes only when `entities.embedding`
/// is populated (Ollama opt-in, §5.3). Where both produce the same pair, the
/// embedding score wins — same precedence as `scan::score::score_pair`.
pub async fn merge_suggestions(
    pool: &PgPool,
    threshold: f32,
    limit: i64,
) -> Result<Vec<MergeSuggestion>, ApiError> {
    let live = sqlx::query(
        "SELECT id, name, kind::text AS kind, (embedding IS NOT NULL) AS has_embedding \
         FROM entities WHERE merged_into_entity_id IS NULL ORDER BY name LIMIT $1",
    )
    .bind(TEXT_PASS_ENTITY_CAP)
    .fetch_all(pool)
    .await?;

    let entities: Vec<(EntityRef, bool)> = live
        .iter()
        .map(|r| {
            (
                EntityRef {
                    id: r.get("id"),
                    name: r.get("name"),
                    kind: r.get("kind"),
                },
                r.get::<bool, _>("has_embedding"),
            )
        })
        .collect();

    // Pairs the reviewer already rejected, in both orderings.
    let dismissed: HashSet<(Uuid, Uuid)> = sqlx::query(
        "SELECT winner_entity_id, loser_entity_id FROM entity_merge_audit WHERE action = 'dismiss'",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .flat_map(|r| {
        let (a, b): (Uuid, Uuid) = (r.get("winner_entity_id"), r.get("loser_entity_id"));
        [(a, b), (b, a)]
    })
    .collect();

    let mut scored: Vec<MergeSuggestion> = Vec::new();
    let mut seen: HashSet<(Uuid, Uuid)> = HashSet::new();

    // --- embedding pass (only contributes when embeddings exist) -----------
    let embedded = entities.iter().filter(|(_, has)| *has).count();
    if embedded >= 2 {
        let rows = sqlx::query(
            r#"
            SELECT a.id AS a_id, b.id AS b_id,
                   (1 - (a.embedding <=> b.embedding))::float4 AS cosine_sim
            FROM entities a
            JOIN entities b ON a.id < b.id
            WHERE a.merged_into_entity_id IS NULL AND b.merged_into_entity_id IS NULL
              AND a.embedding IS NOT NULL AND b.embedding IS NOT NULL
              AND (a.embedding <=> b.embedding) < $1
            ORDER BY a.embedding <=> b.embedding
            LIMIT $2
            "#,
        )
        .bind(EMBEDDING_MAX_DISTANCE)
        .bind(limit)
        .fetch_all(pool)
        .await?;

        for row in rows {
            let (a_id, b_id): (Uuid, Uuid) = (row.get("a_id"), row.get("b_id"));
            let score: f32 = row.get("cosine_sim");
            if score < threshold || dismissed.contains(&(a_id, b_id)) {
                continue;
            }
            let (Some(a), Some(b)) = (find(&entities, a_id), find(&entities, b_id)) else {
                continue; // outside the capped window
            };
            seen.insert((a_id, b_id));
            scored.push(MergeSuggestion {
                a,
                b,
                score,
                method: similarity::pair_method(Some(score)),
            });
        }
    }

    // --- text pass (always runs) -------------------------------------------
    for i in 0..entities.len() {
        for j in (i + 1)..entities.len() {
            let (a, b) = (&entities[i].0, &entities[j].0);
            // Different explicit kinds are unlikely to be the same thing.
            // Everything the extractor creates is 'other' (persist.rs), so
            // this only separates deliberately typed entities.
            if a.kind != b.kind && a.kind != "other" && b.kind != "other" {
                continue;
            }
            if seen.contains(&(a.id, b.id)) || seen.contains(&(b.id, a.id)) {
                continue;
            }
            if dismissed.contains(&(a.id, b.id)) {
                continue;
            }
            let score = similarity::name_similarity(&a.name, &b.name);
            if score < threshold {
                continue;
            }
            scored.push(MergeSuggestion {
                a: a.clone(),
                b: b.clone(),
                score,
                method: similarity::pair_method(None),
            });
        }
    }

    scored.sort_by(|x, y| y.score.total_cmp(&x.score));
    scored.truncate(limit.max(0) as usize);
    Ok(scored)
}

fn find(entities: &[(EntityRef, bool)], id: Uuid) -> Option<EntityRef> {
    entities
        .iter()
        .find(|(e, _)| e.id == id)
        .map(|(e, _)| e.clone())
}

/// Backfill `entities.embedding` for live entities missing one.
///
/// `entities_embedding_hnsw` has existed since 0001 but indexed an all-NULL
/// column, because nothing ever wrote entity embeddings. Same shape and
/// graceful-degradation contract as `extract::persist::embed_pending_segments`:
/// embeddings are an enhancement, so callers treat failure as non-fatal.
pub async fn embed_pending_entities(
    pool: &PgPool,
    ollama: &OllamaClient,
    batch: i64,
) -> Result<usize, String> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, name FROM entities \
         WHERE embedding IS NULL AND merged_into_entity_id IS NULL ORDER BY id LIMIT $1",
    )
    .bind(batch)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = rows.iter().map(|(_, n)| n.clone()).collect();
    let embeddings = ollama.embed(&texts).await?;
    let mut updated = 0usize;
    for ((id, _), embedding) in rows.iter().zip(embeddings) {
        sqlx::query("UPDATE entities SET embedding = $2 WHERE id = $1")
            .bind(id)
            .bind(Vector::from(embedding))
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        updated += 1;
    }
    Ok(updated)
}
