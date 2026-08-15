//! Entity resolution API: list entities, review suggested duplicates, merge
//! them, and add aliases directly.
//!
//! Merging is transactional and always leaves an audit trail, the same
//! contract the contradiction review workflow follows: an entity_merge_audit
//! row records who / when / why, and the losing entity is soft-deleted via
//! merged_into_entity_id rather than dropped, so provenance survives and the
//! export bundle stays round-trippable.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::entities;
use crate::error::ApiError;
use crate::AppState;

#[derive(Deserialize)]
pub struct EntityListParams {
    /// Case-insensitive substring match on name.
    pub q: Option<String>,
    pub kind: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Include entities that have been merged away (default false).
    pub include_merged: Option<bool>,
}

pub async fn list_entities(
    State(state): State<AppState>,
    Query(params): Query<EntityListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);
    let include_merged = params.include_merged.unwrap_or(false);

    let rows = sqlx::query(
        r#"
        SELECT e.id, e.name, e.kind::text AS kind, e.description,
               e.merged_into_entity_id, e.created_at,
               (SELECT count(*) FROM entity_aliases a WHERE a.entity_id = e.id) AS alias_count,
               (SELECT count(*) FROM relationships r
                 WHERE r.source_entity_id = e.id OR r.target_entity_id = e.id) AS edge_count
        FROM entities e
        WHERE ($1 OR e.merged_into_entity_id IS NULL)
          AND ($2::text IS NULL OR e.name ILIKE '%' || $2 || '%')
          AND ($3::text IS NULL OR e.kind::text = $3)
        ORDER BY e.name
        LIMIT $4 OFFSET $5
        "#,
    )
    .bind(include_merged)
    .bind(&params.q)
    .bind(&params.kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "description": r.get::<Option<String>, _>("description"),
                "merged_into_entity_id": r.get::<Option<Uuid>, _>("merged_into_entity_id"),
                "alias_count": r.get::<i64, _>("alias_count"),
                "edge_count": r.get::<i64, _>("edge_count"),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            })
        })
        .collect();

    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct SuggestionParams {
    /// Minimum score to surface; defaults to entities::DEFAULT_THRESHOLD.
    pub threshold: Option<f32>,
    pub limit: Option<i64>,
}

pub async fn list_merge_suggestions(
    State(state): State<AppState>,
    Query(params): Query<SuggestionParams>,
) -> Result<Json<Value>, ApiError> {
    let threshold = params
        .threshold
        .unwrap_or(entities::DEFAULT_THRESHOLD)
        .clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let items = entities::merge_suggestions(&state.pool, threshold, limit).await?;
    Ok(Json(json!({ "items": items, "threshold": threshold })))
}

/// Aliases and merge history for one entity — the detail view behind a row.
pub async fn get_entity(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        "SELECT id, name, kind::text AS kind, description, merged_into_entity_id, created_at \
         FROM entities WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("entity {id}")))?;

    let aliases: Vec<String> =
        sqlx::query_scalar("SELECT alias FROM entity_aliases WHERE entity_id = $1 ORDER BY alias")
            .bind(id)
            .fetch_all(&state.pool)
            .await?;

    let audit = sqlx::query(
        "SELECT action, actor, note, created_at, winner_entity_id, loser_entity_id \
         FROM entity_merge_audit \
         WHERE winner_entity_id = $1 OR loser_entity_id = $1 ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "name": row.get::<String, _>("name"),
        "kind": row.get::<String, _>("kind"),
        "description": row.get::<Option<String>, _>("description"),
        "merged_into_entity_id": row.get::<Option<Uuid>, _>("merged_into_entity_id"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "aliases": aliases,
        "audit": audit.iter().map(|a| json!({
            "action": a.get::<String, _>("action"),
            "actor": a.get::<String, _>("actor"),
            "note": a.get::<Option<String>, _>("note"),
            "winner_entity_id": a.get::<Uuid, _>("winner_entity_id"),
            "loser_entity_id": a.get::<Uuid, _>("loser_entity_id"),
            "created_at": a.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct MergeRequest {
    /// The entity folded into the one named in the path.
    pub loser_id: Uuid,
    pub note: Option<String>,
    /// Reviewer identity; defaults to 'local-user' (single-user desktop).
    pub actor: Option<String>,
}

/// POST /entities/{id}/merge — `{id}` survives, `loser_id` is folded into it.
pub async fn merge_entity(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<Value>, ApiError> {
    let outcome =
        entities::merge_entities(&state.pool, id, req.loser_id, req.note, req.actor).await?;
    Ok(Json(json!(outcome)))
}

#[derive(Deserialize)]
pub struct DismissRequest {
    pub other_id: Uuid,
    pub note: Option<String>,
    pub actor: Option<String>,
}

/// Reject a suggested pair so it stops being offered, without changing either
/// entity.
pub async fn dismiss_merge_suggestion(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<DismissRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    entities::dismiss_suggestion(&state.pool, id, req.other_id, req.note, req.actor).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "dismissed": [id, req.other_id] })),
    ))
}

#[derive(Deserialize)]
pub struct AliasRequest {
    pub alias: String,
}

/// Add an alias directly, without merging — for the case where the duplicate
/// has not been ingested as its own entity yet, so a later sighting of that
/// name resolves straight to this entity.
pub async fn add_alias(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<AliasRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let alias = req.alias.trim();
    if alias.is_empty() {
        return Err(ApiError::BadRequest("alias must not be empty".to_string()));
    }

    // A client holding a stale id could otherwise hang the alias on a
    // merged-away entity, which resolve_or_create_entity would then hand back
    // — reviving the node the merge retired.
    let target: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT merged_into_entity_id FROM entities WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    match target {
        None => return Err(ApiError::NotFound(format!("entity {id}"))),
        Some(Some(head)) => {
            return Err(ApiError::BadRequest(format!(
                "entity {id} has been merged into {head}; alias that entity instead"
            )))
        }
        Some(None) => {}
    }

    // An alias that already resolves to a different live entity — whether as
    // that entity's name or as one of its aliases — would make
    // resolve_or_create_entity's `UNION … LIMIT 1` nondeterministic. Such a
    // pair should be merged instead.
    let clash: Option<Uuid> = sqlx::query_scalar(
        "SELECT e.id FROM entities e \
           WHERE lower(e.name) = lower($1) AND e.id <> $2 AND e.merged_into_entity_id IS NULL \
         UNION \
         SELECT a.entity_id FROM entity_aliases a \
           JOIN entities e2 ON e2.id = a.entity_id \
           WHERE lower(a.alias) = lower($1) AND a.entity_id <> $2 \
             AND e2.merged_into_entity_id IS NULL \
         LIMIT 1",
    )
    .bind(alias)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(other) = clash {
        return Err(ApiError::BadRequest(format!(
            "'{alias}' already resolves to entity {other}; merge them instead"
        )));
    }

    let inserted = sqlx::query(
        "INSERT INTO entity_aliases (entity_id, alias) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(alias)
    .execute(&state.pool)
    .await?
    .rows_affected();

    Ok((
        StatusCode::CREATED,
        Json(json!({ "entity_id": id, "alias": alias, "added": inserted > 0 })),
    ))
}
