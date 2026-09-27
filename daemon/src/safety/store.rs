//! Persistence and queries for inference certificates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use super::certificate::{InferenceCertificate, Outcome};
use super::reason::ReasonCode;
use crate::error::ApiError;

/// Persist a certificate, returning its id. Idempotent: the same evidence
/// under the same rule for the same conclusion returns the existing live
/// certificate. A live certificate for the same conclusion and rule that
/// rested on *different* evidence is superseded by this one, so the history
/// of how a conclusion's support changed is kept.
pub async fn record(
    conn: &mut PgConnection,
    cert: &InferenceCertificate,
) -> Result<Uuid, ApiError> {
    let digest = cert.evidence_digest();
    let codes: Vec<String> = cert
        .reason_codes()
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();
    let input_ids: Vec<Uuid> = cert.inputs.iter().map(|e| e.id).collect();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO inference_certificates \
           (conclusion_kind, conclusion_key, conclusion_id, subject_ids, rule_id, rule_version, \
            decision, outcome, evidence_class, inputs, input_ids, source_artifact_ids, \
            source_family_ids, model_version, config, scope, temporal, predicates, reason_codes, \
            explanation, evidence_digest) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                 $17, $18, $19, $20) \
         ON CONFLICT (rule_id, conclusion_key, evidence_digest) \
           WHERE superseded_at IS NULL AND retracted_at IS NULL DO NOTHING \
         RETURNING id",
    )
    .bind(cert.kind.as_str())
    .bind(&cert.conclusion_key)
    .bind(cert.conclusion_id)
    .bind(&cert.subject_ids)
    .bind(&cert.rule_id)
    .bind(cert.rule_version)
    .bind(cert.decision.as_str())
    .bind(
        tj(&cert.evidence_class)?
            .as_str()
            .unwrap_or("inferred")
            .to_string(),
    )
    .bind(tj(&cert.inputs)?)
    .bind(&input_ids)
    .bind(&cert.source_artifact_ids)
    .bind(&cert.source_family_ids)
    .bind(&cert.model_version)
    .bind(null_to_object(&cert.config))
    .bind(null_to_object(&cert.scope))
    .bind(null_to_object(&cert.temporal))
    .bind(tj(&cert.predicates)?)
    .bind(&codes)
    .bind(&cert.explanation)
    .bind(&digest)
    .fetch_optional(&mut *conn)
    .await?;
    let id = match inserted {
        Some(id) => {
            sqlx::query(
                "UPDATE inference_certificates SET superseded_at = now(), outcome = 'superseded', \
                   status_reason = 'the evidence behind this conclusion changed' \
                 WHERE rule_id = $1 AND conclusion_key = $2 AND id <> $3 \
                   AND superseded_at IS NULL AND retracted_at IS NULL",
            )
            .bind(&cert.rule_id)
            .bind(&cert.conclusion_key)
            .bind(id)
            .execute(&mut *conn)
            .await?;
            metrics::counter!(
                "gather_inference_certificates_total",
                "kind" => cert.kind.as_str(),
                "decision" => cert.decision.as_str()
            )
            .increment(1);
            id
        }
        None => {
            sqlx::query_scalar(
                "SELECT id FROM inference_certificates \
                 WHERE rule_id = $1 AND conclusion_key = $2 AND evidence_digest = $3 \
                   AND superseded_at IS NULL AND retracted_at IS NULL",
            )
            .bind(&cert.rule_id)
            .bind(&cert.conclusion_key)
            .bind(&digest)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    if let Some(cid) = cert.conclusion_id {
        set_conclusion(conn, id, cid).await?;
    }
    Ok(id)
}

fn tj<T: Serialize>(v: &T) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::Internal(e.into()))
}

fn null_to_object(v: &Value) -> Value {
    if v.is_null() {
        json!({})
    } else {
        v.clone()
    }
}

/// Link a certificate to the row that materialized its conclusion.
pub async fn set_conclusion(
    conn: &mut PgConnection,
    id: Uuid,
    conclusion_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE inference_certificates SET conclusion_id = $2 WHERE id = $1")
        .bind(id)
        .bind(conclusion_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Withdraw certificates: `retracted` when their evidence is gone,
/// `superseded` when newer evidence replaced them. Returns the ids changed.
pub async fn withdraw(
    conn: &mut PgConnection,
    ids: &[Uuid],
    outcome: Outcome,
    reason: &str,
    caused_by: Option<Uuid>,
) -> Result<Vec<Uuid>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = match outcome {
        Outcome::Retracted => {
            "UPDATE inference_certificates SET retracted_at = now(), outcome = 'retracted', \
               status_reason = $2, caused_by = $3 \
             WHERE id = ANY($1) AND superseded_at IS NULL AND retracted_at IS NULL RETURNING id"
        }
        _ => {
            "UPDATE inference_certificates SET superseded_at = now(), outcome = 'superseded', \
               status_reason = $2, caused_by = $3 \
             WHERE id = ANY($1) AND superseded_at IS NULL AND retracted_at IS NULL RETURNING id"
        }
    };
    Ok(sqlx::query_scalar(sql)
        .bind(ids)
        .bind(reason)
        .bind(caused_by)
        .fetch_all(&mut *conn)
        .await?)
}

/// Live certificates of `kind` whose subjects include any of `subjects`.
pub async fn live_for_subjects(
    conn: &mut PgConnection,
    kind: &str,
    subjects: &[Uuid],
    decision: Option<&str>,
) -> Result<Vec<(Uuid, String, Vec<Uuid>)>, ApiError> {
    let rows = sqlx::query(
        "SELECT id, conclusion_key, subject_ids FROM inference_certificates \
         WHERE conclusion_kind = $1 AND subject_ids && $2 \
           AND ($3::text IS NULL OR decision = $3) \
           AND superseded_at IS NULL AND retracted_at IS NULL",
    )
    .bind(kind)
    .bind(subjects)
    .bind(decision)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("id"), r.get("conclusion_key"), r.get("subject_ids")))
        .collect())
}

// ---------------------------------------------------------------------------
// Read side

#[derive(Debug, Clone, Serialize)]
pub struct ReasonView {
    pub code: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CertificateView {
    pub id: Uuid,
    pub conclusion_kind: String,
    pub conclusion_key: String,
    pub conclusion_id: Option<Uuid>,
    pub subject_ids: Vec<Uuid>,
    pub rule_id: String,
    pub rule_version: i32,
    pub decision: String,
    pub outcome: String,
    pub evidence_class: String,
    pub inputs: Value,
    pub source_artifact_ids: Vec<Uuid>,
    pub source_family_ids: Vec<Uuid>,
    pub model_version: Option<String>,
    pub config: Value,
    pub scope: Value,
    pub temporal: Value,
    pub predicates: Value,
    pub reason_codes: Vec<String>,
    /// The reason codes in plain language.
    pub reasons: Vec<ReasonView>,
    pub explanation: String,
    pub evidence_digest: String,
    pub created_at: DateTime<Utc>,
    pub superseded_at: Option<DateTime<Utc>>,
    pub retracted_at: Option<DateTime<Utc>>,
    pub status_reason: Option<String>,
    pub caused_by: Option<Uuid>,
}

macro_rules! columns {
    () => {
        "id, conclusion_kind, conclusion_key, conclusion_id, subject_ids, rule_id, \
    rule_version, decision, outcome, evidence_class, inputs, source_artifact_ids, \
    source_family_ids, model_version, config, scope, temporal, predicates, reason_codes, \
    explanation, evidence_digest, created_at, superseded_at, retracted_at, status_reason, caused_by"
    };
}

fn view(r: &sqlx::postgres::PgRow) -> CertificateView {
    let codes: Vec<String> = r.get("reason_codes");
    CertificateView {
        id: r.get("id"),
        conclusion_kind: r.get("conclusion_kind"),
        conclusion_key: r.get("conclusion_key"),
        conclusion_id: r.get("conclusion_id"),
        subject_ids: r.get("subject_ids"),
        rule_id: r.get("rule_id"),
        rule_version: r.get("rule_version"),
        decision: r.get("decision"),
        outcome: r.get("outcome"),
        evidence_class: r.get("evidence_class"),
        inputs: r.get("inputs"),
        source_artifact_ids: r.get("source_artifact_ids"),
        source_family_ids: r.get("source_family_ids"),
        model_version: r.get("model_version"),
        config: r.get("config"),
        scope: r.get("scope"),
        temporal: r.get("temporal"),
        predicates: r.get("predicates"),
        reasons: codes
            .iter()
            .map(|c| ReasonView {
                code: c.clone(),
                text: ReasonCode::parse(c)
                    .map(|r| r.plain_language().to_string())
                    .unwrap_or_default(),
            })
            .collect(),
        reason_codes: codes,
        explanation: r.get("explanation"),
        evidence_digest: r.get::<String, _>("evidence_digest"),
        created_at: r.get("created_at"),
        superseded_at: r.get("superseded_at"),
        retracted_at: r.get("retracted_at"),
        status_reason: r.get("status_reason"),
        caused_by: r.get("caused_by"),
    }
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<CertificateView, ApiError> {
    let row = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM inference_certificates WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("certificate {id}")))?;
    Ok(view(&row))
}

/// Filters for listing certificates; all optional and combined with AND.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CertificateFilter {
    /// The conclusion's row id, or any id the conclusion is about.
    pub conclusion_id: Option<Uuid>,
    pub subject_id: Option<Uuid>,
    /// Conclusions derived from this source artifact.
    pub artifact_id: Option<Uuid>,
    /// A reason code, e.g. CHAINED_SIMILARITY.
    pub reason: Option<String>,
    /// auto_applied | needs_review | blocked | user_decision | superseded | retracted
    pub outcome: Option<String>,
    pub kind: Option<String>,
    pub rule: Option<String>,
    /// Only certificates that are still in force.
    pub live: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list(pool: &PgPool, f: &CertificateFilter) -> Result<Vec<CertificateView>, ApiError> {
    if let Some(r) = &f.reason {
        if ReasonCode::parse(r).is_none() {
            return Err(ApiError::BadRequest(format!("unknown reason code '{r}'")));
        }
    }
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM inference_certificates \
         WHERE ($1::uuid IS NULL OR conclusion_id = $1 OR $1 = ANY(subject_ids)) \
           AND ($2::uuid IS NULL OR $2 = ANY(subject_ids)) \
           AND ($3::uuid IS NULL OR $3 = ANY(source_artifact_ids)) \
           AND ($4::text IS NULL OR $4 = ANY(reason_codes)) \
           AND ($5::text IS NULL OR outcome = $5) \
           AND ($6::text IS NULL OR conclusion_kind = $6) \
           AND ($7::text IS NULL OR rule_id = $7) \
           AND (NOT $8 OR (superseded_at IS NULL AND retracted_at IS NULL)) \
         ORDER BY created_at DESC, id LIMIT $9 OFFSET $10"
    ))
    .bind(f.conclusion_id)
    .bind(f.subject_id)
    .bind(f.artifact_id)
    .bind(&f.reason)
    .bind(&f.outcome)
    .bind(&f.kind)
    .bind(&f.rule)
    .bind(f.live.unwrap_or(false))
    .bind(f.limit.unwrap_or(100).clamp(1, 500))
    .bind(f.offset.unwrap_or(0).max(0))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(view).collect())
}

/// Every certificate withdrawn, directly or transitively, because of `id`
/// (a split, a "not a duplicate", a removed source).
pub async fn affected_by(pool: &PgPool, id: Uuid) -> Result<Vec<CertificateView>, ApiError> {
    let rows = sqlx::query(concat!(
        "WITH RECURSIVE hit(id, depth) AS ( \
             SELECT id, 1 FROM inference_certificates WHERE caused_by = $1 \
           UNION \
             SELECT c.id, h.depth + 1 FROM inference_certificates c JOIN hit h ON c.caused_by = h.id \
             WHERE h.depth < 16 \
         ) \
         SELECT ", columns!(), " FROM inference_certificates WHERE id IN (SELECT id FROM hit) \
         ORDER BY created_at, id"
    ))
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(view).collect())
}

/// The causal chain behind `id`: the certificate, what caused its
/// withdrawal, what caused that, and every other certificate for the same
/// conclusion (its history).
pub async fn chain(pool: &PgPool, id: Uuid) -> Result<Value, ApiError> {
    let cert = get(pool, id).await?;
    let mut causes = Vec::new();
    let mut next = cert.caused_by;
    while let Some(c) = next {
        if causes.len() >= 16 {
            break;
        }
        let v = get(pool, c).await?;
        next = v.caused_by;
        causes.push(v);
    }
    let rows = sqlx::query(concat!(
        "SELECT ",
        columns!(),
        " FROM inference_certificates WHERE conclusion_key = $1 \
         ORDER BY created_at, id LIMIT 100"
    ))
    .bind(&cert.conclusion_key)
    .fetch_all(pool)
    .await?;
    let affected = affected_by(pool, id).await?;
    Ok(json!({
        "certificate": cert,
        "caused_by": causes,
        "history": rows.iter().map(view).collect::<Vec<_>>(),
        "affected": affected,
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct SafetySummary {
    pub by_outcome: Value,
    pub review_by_reason: Value,
    pub blocked_by_reason: Value,
    pub by_kind: Value,
}

pub async fn summary(pool: &PgPool) -> Result<SafetySummary, ApiError> {
    let pairs = |rows: Vec<(String, i64)>| {
        Value::Object(rows.into_iter().map(|(k, n)| (k, json!(n))).collect())
    };
    let by_outcome: Vec<(String, i64)> = sqlx::query_as(
        "SELECT outcome, count(*) FROM inference_certificates GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    let reasons = |outcome: &'static str| {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT r, count(*) FROM inference_certificates, unnest(reason_codes) r \
             WHERE outcome = $1 GROUP BY 1 ORDER BY 1",
        )
        .bind(outcome)
        .fetch_all(pool)
    };
    let review = reasons("needs_review").await?;
    let blocked = reasons("blocked").await?;
    let by_kind: Vec<(String, i64)> = sqlx::query_as(
        "SELECT conclusion_kind, count(*) FROM inference_certificates \
         WHERE superseded_at IS NULL AND retracted_at IS NULL GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(SafetySummary {
        by_outcome: pairs(by_outcome),
        review_by_reason: pairs(review),
        blocked_by_reason: pairs(blocked),
        by_kind: pairs(by_kind),
    })
}
