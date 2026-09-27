//! Contradictions the safety layer explained away, and a person's review of
//! them.
//!
//! When the alignment rule finds a known reason two flagged claims can both
//! be true (different periods, scopes, places or modality) it reports
//! nothing and records a blocked certificate. That reason rests on what was
//! extracted; a misread date turns a real conflict into "history". This
//! module lists those pairs so a person can spot-check them, and records the
//! verdict as evidence no later scan overrides:
//!
//! - **confirm** ("this is a real conflict") reports the pair as an open
//!   contradiction, undoes a supersession the rule applied between the two
//!   claims, and withdraws the blocked certificate;
//! - **agree** ("the explanation is right") takes the pair off the list and
//!   counts as "not a conflict" from then on.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use super::certificate::{EvidenceClass, Outcome};
use super::contradiction::RULE_CONTRADICTION;
use super::reason::ReasonCode;
use super::service::{undo_supersession, user_decision};
use super::store::{self, ReasonView};
use crate::error::ApiError;

pub const CONFIRMED: &str = "contradiction_confirmed";
pub const NOT_CONFLICT: &str = "contradiction_not_conflict";
/// `resolved_by` on a contradiction closed by agreeing with its explanation:
/// "not a conflict", but the change of state the explanation names still
/// holds (unlike a plain "both valid").
pub const AGREED_BY: &str = "explained-away";

#[derive(Debug, Clone, Serialize)]
pub struct ClaimView {
    pub id: Uuid,
    pub statement: String,
    pub status: String,
    pub valid_from: Option<DateTime<Utc>>,
    pub superseded_by_unit_id: Option<Uuid>,
}

/// One pair Gather decided was not a contradiction, and why.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainedAway {
    pub certificate_id: Uuid,
    pub unit_a: ClaimView,
    pub unit_b: ClaimView,
    /// Why the pair was not reported, in plain language.
    pub reasons: Vec<ReasonView>,
    pub explanation: String,
    pub detection_method: String,
    pub score: f64,
    pub decided_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainedAwayPage {
    pub items: Vec<ExplainedAway>,
    pub total: i64,
}

// Live blocked contradiction certificates about two claims that still exist
// and haven't been reviewed. Pairs blocked only because a person already
// resolved them are left out: that decision is already theirs.
macro_rules! pending_from {
    () => {
        " FROM inference_certificates c \
         JOIN atomic_units ua ON ua.id = LEAST(c.subject_ids[1], c.subject_ids[2]) \
         JOIN atomic_units ub ON ub.id = GREATEST(c.subject_ids[1], c.subject_ids[2]) \
         WHERE c.rule_id = 'contradiction.aligned_conflict' AND c.outcome = 'blocked' \
           AND c.superseded_at IS NULL AND c.retracted_at IS NULL \
           AND cardinality(c.subject_ids) = 2 \
           AND NOT ('USER_REJECTION_EXISTS' = ANY(c.reason_codes)) \
           AND ua.status IN ('active', 'superseded') AND ub.status IN ('active', 'superseded') \
           AND NOT EXISTS (SELECT 1 FROM semantic_user_decisions d \
                           WHERE d.kind IN ('contradiction_confirmed', 'contradiction_not_conflict') \
                             AND d.a_id = ua.id AND d.b_id = ub.id AND d.revoked_at IS NULL)"
    };
}

/// The pairs waiting for a spot-check, newest first.
pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> Result<ExplainedAwayPage, ApiError> {
    let total: i64 = sqlx::query_scalar(concat!("SELECT count(*)", pending_from!()))
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query(concat!(
        "SELECT c.id, c.reason_codes, c.explanation, c.config, c.created_at, \
           ua.id AS a_id, ua.statement AS a_statement, ua.status::text AS a_status, \
           ua.valid_from AS a_valid_from, ua.superseded_by_unit_id AS a_superseded_by, \
           ub.id AS b_id, ub.statement AS b_statement, ub.status::text AS b_status, \
           ub.valid_from AS b_valid_from, ub.superseded_by_unit_id AS b_superseded_by",
        pending_from!(),
        " ORDER BY c.created_at DESC, c.id LIMIT $1 OFFSET $2"
    ))
    .bind(limit.clamp(1, 1000))
    .bind(offset.max(0))
    .fetch_all(pool)
    .await?;
    let claim = |r: &sqlx::postgres::PgRow, p: &str| ClaimView {
        id: r.get(format!("{p}_id").as_str()),
        statement: r.get(format!("{p}_statement").as_str()),
        status: r.get(format!("{p}_status").as_str()),
        valid_from: r.get(format!("{p}_valid_from").as_str()),
        superseded_by_unit_id: r.get(format!("{p}_superseded_by").as_str()),
    };
    let items = rows
        .iter()
        .map(|r| {
            let codes: Vec<String> = r.get("reason_codes");
            let config: Value = r.get("config");
            ExplainedAway {
                certificate_id: r.get("id"),
                unit_a: claim(r, "a"),
                unit_b: claim(r, "b"),
                reasons: codes
                    .iter()
                    .map(|c| ReasonView {
                        code: c.clone(),
                        text: ReasonCode::parse(c)
                            .map(|x| x.plain_language().to_string())
                            .unwrap_or_default(),
                    })
                    .collect(),
                explanation: r.get("explanation"),
                detection_method: config["rule"].as_str().unwrap_or("").to_string(),
                score: config["structural_score"].as_f64().unwrap_or(0.5),
                decided_at: r.get("created_at"),
            }
        })
        .collect();
    Ok(ExplainedAwayPage { items, total })
}

struct Pending {
    lo: Uuid,
    hi: Uuid,
    config: Value,
    scope: Value,
}

/// Lock a blocked contradiction certificate that is still in force.
async fn pending(conn: &mut PgConnection, cert: Uuid) -> Result<Pending, ApiError> {
    let row = sqlx::query(
        "SELECT subject_ids, config, scope FROM inference_certificates \
         WHERE id = $1 AND rule_id = $2 AND outcome = 'blocked' \
           AND superseded_at IS NULL AND retracted_at IS NULL FOR UPDATE",
    )
    .bind(cert)
    .bind(RULE_CONTRADICTION.id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| {
        ApiError::NotFound(format!(
            "explained-away contradiction {cert} (or already reviewed)"
        ))
    })?;
    let subjects: Vec<Uuid> = row.get("subject_ids");
    let [x, y] = subjects[..] else {
        return Err(ApiError::BadRequest(
            "certificate is not about a pair".into(),
        ));
    };
    Ok(Pending {
        lo: x.min(y),
        hi: x.max(y),
        config: row.get("config"),
        scope: row.get("scope"),
    })
}

async fn decide(
    conn: &mut PgConnection,
    kind: &str,
    p: &Pending,
    note: &Option<String>,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO semantic_user_decisions (kind, a_id, b_id, note) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(kind)
    .bind(p.lo)
    .bind(p.hi)
    .bind(note)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// "This is a real conflict." Returns the contradiction now open for it.
pub async fn confirm(pool: &PgPool, cert: Uuid, note: Option<String>) -> Result<Value, ApiError> {
    let mut tx = pool.begin().await?;
    let p = pending(&mut tx, cert).await?;
    sqlx::query(
        "UPDATE semantic_user_decisions SET revoked_at = now() \
         WHERE kind = $1 AND a_id = $2 AND b_id = $3 AND revoked_at IS NULL",
    )
    .bind(NOT_CONFLICT)
    .bind(p.lo)
    .bind(p.hi)
    .execute(&mut *tx)
    .await?;
    decide(&mut tx, CONFIRMED, &p, &note).await?;
    let earlier: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM inference_certificates \
         WHERE rule_id = 'user.contradiction_not_conflict' AND conclusion_key = $1 \
           AND superseded_at IS NULL AND retracted_at IS NULL",
    )
    .bind(format!("user-not-contradiction:{}:{}", p.lo, p.hi))
    .fetch_all(&mut *tx)
    .await?;
    let event = store::record(
        &mut tx,
        &user_decision(
            "user.contradiction_confirmed",
            format!("user-contradiction:{}:{}", p.lo, p.hi),
            vec![p.lo, p.hi],
            EvidenceClass::UserConfirmed,
            "You marked these statements as a real conflict; Gather won't explain it away again."
                .into(),
        ),
    )
    .await?;

    // If the rule read the pair as a change of state, the older claim was
    // marked superseded; a real conflict means it was never replaced.
    let supersessions = sqlx::query(
        "SELECT id, scope FROM inference_certificates \
         WHERE conclusion_kind = 'fact_supersession' AND decision = 'auto_applied' \
           AND subject_ids @> $1 AND superseded_at IS NULL AND retracted_at IS NULL",
    )
    .bind(vec![p.lo, p.hi])
    .fetch_all(&mut *tx)
    .await?;
    let mut reverted = 0;
    let mut undone = Vec::new();
    for r in &supersessions {
        reverted += undo_supersession(&mut tx, &r.get::<Value, _>("scope")).await?;
        undone.push(r.get::<Uuid, _>("id"));
    }
    let reason = "you marked these statements as a real conflict";
    store::withdraw(&mut tx, &earlier, Outcome::Superseded, reason, Some(event)).await?;
    store::withdraw(&mut tx, &undone, Outcome::Retracted, reason, Some(event)).await?;
    store::withdraw(&mut tx, &[cert], Outcome::Superseded, reason, Some(event)).await?;

    let method = p.config["rule"].as_str().unwrap_or("user");
    let score = p.config["structural_score"]
        .as_f64()
        .unwrap_or(0.5)
        .clamp(0.0, 1.0) as f32;
    let explanation = match &note {
        Some(n) if !n.trim().is_empty() => format!("You marked this as a real conflict: {n}"),
        _ => "You marked this as a real conflict.".to_string(),
    };
    let (id, previous): (Uuid, Option<String>) = sqlx::query_as(
        "WITH prev AS (SELECT status::text AS s FROM contradictions \
                       WHERE unit_a_id = $1 AND unit_b_id = $2) \
         INSERT INTO contradictions (unit_a_id, unit_b_id, score, detection_method, explanation, \
                                     certificate_id, alignment, certainty) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'user_confirmed') \
         ON CONFLICT (unit_a_id, unit_b_id) DO UPDATE SET status = 'open', resolved_at = NULL, \
           resolved_by = NULL, resolution_note = NULL, certificate_id = EXCLUDED.certificate_id, \
           certainty = 'user_confirmed', score = EXCLUDED.score, \
           detection_method = EXCLUDED.detection_method, explanation = EXCLUDED.explanation, \
           alignment = EXCLUDED.alignment \
         RETURNING id, (SELECT s FROM prev)",
    )
    .bind(p.lo)
    .bind(p.hi)
    .bind(score)
    .bind(method)
    .bind(&explanation)
    .bind(event)
    .bind(p.scope.get("alignment").cloned().unwrap_or(Value::Null))
    .fetch_one(&mut *tx)
    .await?;
    store::set_conclusion(&mut tx, event, id).await?;
    sqlx::query(
        "INSERT INTO contradiction_audit (contradiction_id, action, actor, from_status, \
           to_status, note) VALUES ($1, 'confirm', 'local-user', $2::contradiction_status, \
           'open', $3)",
    )
    .bind(id)
    .bind(previous)
    .bind(&explanation)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({
        "contradiction_id": id,
        "certificate": event,
        "supersessions_reverted": reverted,
    }))
}

/// "The explanation is right." The pair counts as not a conflict.
pub async fn agree(pool: &PgPool, cert: Uuid, note: Option<String>) -> Result<Value, ApiError> {
    let mut tx = pool.begin().await?;
    let p = pending(&mut tx, cert).await?;
    decide(&mut tx, NOT_CONFLICT, &p, &note).await?;
    // A pair can still have an open contradiction from an earlier reading
    // (before an edit changed its time or scope); the verdict closes it.
    let resolution = match &note {
        Some(n) if !n.trim().is_empty() => format!("You agreed this is not a conflict: {n}"),
        _ => "You agreed this is not a conflict.".to_string(),
    };
    let closed: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE contradictions SET status = 'both_valid', resolved_at = now(), \
           resolved_by = $3, resolution_note = $4 \
         WHERE unit_a_id = $1 AND unit_b_id = $2 AND status = 'open' RETURNING id",
    )
    .bind(p.lo)
    .bind(p.hi)
    .bind(AGREED_BY)
    .bind(&resolution)
    .fetch_all(&mut *tx)
    .await?;
    for id in &closed {
        sqlx::query(
            "INSERT INTO contradiction_audit (contradiction_id, action, actor, from_status, \
               to_status, note) VALUES ($1, 'resolve', 'local-user', 'open', 'both_valid', $2)",
        )
        .bind(id)
        .bind(&resolution)
        .execute(&mut *tx)
        .await?;
    }
    let event = store::record(
        &mut tx,
        &user_decision(
            "user.contradiction_not_conflict",
            format!("user-not-contradiction:{}:{}", p.lo, p.hi),
            vec![p.lo, p.hi],
            EvidenceClass::Rejected,
            "You agreed these statements don't conflict.".into(),
        ),
    )
    .await?;
    if let Some(id) = closed.first() {
        store::set_conclusion(&mut tx, event, *id).await?;
    }
    tx.commit().await?;
    Ok(json!({ "certificate": event, "contradictions_closed": closed.len() }))
}

/// The person's standing verdict on a pair, if any.
pub async fn verdict(
    conn: &mut PgConnection,
    lo: Uuid,
    hi: Uuid,
) -> Result<Option<String>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT kind FROM semantic_user_decisions \
         WHERE kind IN ('contradiction_confirmed', 'contradiction_not_conflict') \
           AND a_id = $1 AND b_id = $2 AND revoked_at IS NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(lo)
    .bind(hi)
    .fetch_optional(&mut *conn)
    .await?)
}
