//! Database-backed safety operations: user decisions as evidence, source
//! retraction with propagation, photo "not a duplicate", declared
//! derivations, claim support and extractor revisions.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use super::certificate::{
    ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, Outcome, RuleId,
};
use super::provenance::{self, Derivation, SourceRef};
use super::store;
use crate::error::ApiError;

/// A certificate for a person's explicit decision. It is evidence (never an
/// inference) and later rules treat it as fixed.
pub fn user_decision(
    rule: &'static str,
    key: String,
    subjects: Vec<Uuid>,
    class: EvidenceClass,
    explanation: String,
) -> InferenceCertificate {
    let mut cert = InferenceCertificate::new(
        ConclusionKind::UserDecision,
        RuleId {
            id: rule,
            version: 1,
        },
        key,
    )
    .with_subjects(subjects.clone());
    for id in subjects {
        cert = cert.input(EvidenceRef {
            id,
            kind: "subject".into(),
            class,
            detail: Value::Null,
        });
    }
    let mut cert = cert.decide(Decision::UserDecision).explain(explanation);
    cert.evidence_class = class;
    cert
}

/// Withdraw the live automatic merge certificates that folded `entity` in.
pub async fn retract_merge_certificates(
    conn: &mut PgConnection,
    entity: Uuid,
    outcome: Outcome,
    reason: &str,
    caused_by: Uuid,
) -> Result<Vec<Uuid>, ApiError> {
    let ids: Vec<Uuid> =
        store::live_for_subjects(conn, "entity_merge", &[entity], Some("auto_applied"))
            .await?
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
    // A person's own earlier merge of this entity is replaced by the split.
    let manual: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM inference_certificates \
         WHERE rule_id = 'user.entity_merge' AND $1 = ANY(subject_ids) \
           AND superseded_at IS NULL AND retracted_at IS NULL",
    )
    .bind(entity)
    .fetch_all(&mut *conn)
    .await?;
    store::withdraw(conn, &manual, Outcome::Superseded, reason, Some(caused_by)).await?;
    store::withdraw(conn, &ids, outcome, reason, Some(caused_by)).await
}

/// Retract every live certificate that can no longer satisfy its rule once
/// `withdrawn` evidence (units, artifacts, certificates) is gone: those that
/// used any of it as a direct input, or with no live source artifact left
/// (withdrawn now, retracted earlier, or deleted).
/// Repeats to a fixpoint so conclusions built on conclusions follow; each
/// is linked to the certificate (or event) that caused it.
pub async fn propagate_withdrawal(
    conn: &mut PgConnection,
    withdrawn: &[Uuid],
    reason: &str,
    event: Uuid,
) -> Result<Vec<Uuid>, ApiError> {
    let mut gone: Vec<Uuid> = withdrawn.to_vec();
    let mut all: Vec<Uuid> = Vec::new();
    let mut previous: Vec<Uuid> = vec![event];
    for _ in 0..8 {
        let hit: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "UPDATE inference_certificates c \
               SET retracted_at = now(), outcome = 'retracted', status_reason = $2, \
                   caused_by = coalesce( \
                     (SELECT p.id FROM inference_certificates p \
                      WHERE p.id = ANY($3) AND p.id <> $4 \
                        AND (p.id = ANY(c.input_ids) OR p.conclusion_id = ANY(c.input_ids)) \
                      LIMIT 1), $4) \
             WHERE c.superseded_at IS NULL AND c.retracted_at IS NULL \
               AND c.decision <> 'user_decision' AND c.id <> $4 \
               AND (c.input_ids && $1 \
                    OR (cardinality(c.source_artifact_ids) > 0 \
                        AND NOT EXISTS ( \
                          SELECT 1 FROM unnest(c.source_artifact_ids) src \
                          JOIN artifacts a ON a.id = src AND a.retracted_at IS NULL \
                          WHERE src <> ALL($1)))) \
             RETURNING c.id, c.conclusion_id",
        )
        .bind(&gone)
        .bind(reason)
        .bind(&previous)
        .bind(event)
        .fetch_all(&mut *conn)
        .await?;
        if hit.is_empty() {
            break;
        }
        previous = hit.iter().map(|(id, _)| *id).collect();
        for (id, conclusion) in hit {
            all.push(id);
            gone.push(id);
            gone.extend(conclusion);
        }
    }
    Ok(all)
}

#[derive(Debug, Default, Serialize)]
pub struct RetractionReport {
    pub event_certificate: Option<Uuid>,
    pub units_retracted: Vec<Uuid>,
    pub certificates_withdrawn: Vec<Uuid>,
    pub contradictions_withdrawn: u64,
    pub supersessions_reverted: u64,
    pub images_ungrouped: u64,
    /// Automatic entity merges undone because no remaining source backs them.
    pub merges_withdrawn: u64,
    pub deleted: bool,
    /// A file the artifact row pointed at outside the database. Gather never
    /// writes such paths itself (they only arrive in imported bundles), so it
    /// never deletes them either: the path is reported for the user.
    pub external_file_left: Option<String>,
}

/// Undo the automatic merges that folded `members` into their heads,
/// newest-first as the journal requires, stopping at a person's merge. Each
/// undo runs in a savepoint so one that can't be done exactly doesn't abort
/// the rest. The next resolution pass re-merges anything still supported.
async fn unwind_auto_merges(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    members: &[Uuid],
    note: &str,
) -> Result<u64, ApiError> {
    use sqlx::Acquire;
    let heads: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT merged_into_entity_id FROM entities \
         WHERE id = ANY($1) AND merged_into_entity_id IS NOT NULL",
    )
    .bind(members)
    .fetch_all(&mut **tx)
    .await?;
    let mut undone = 0;
    for head in heads {
        let mut targets: BTreeSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM entities WHERE id = ANY($1) AND merged_into_entity_id = $2",
        )
        .bind(members)
        .bind(head)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .collect();
        let merges: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT loser_entity_id, actor FROM entity_merge_audit \
             WHERE winner_entity_id = $1 AND action = 'merge' AND undone_at IS NULL \
             ORDER BY created_at DESC",
        )
        .bind(head)
        .fetch_all(&mut **tx)
        .await?;
        for (loser, actor) in merges {
            if targets.is_empty() || actor != "auto" {
                break;
            }
            let mut sp = tx.begin().await?;
            match crate::entities::merge::unmerge_for_supersession_in(
                &mut sp,
                loser,
                note.to_string(),
            )
            .await
            {
                Ok(_) => {
                    sp.commit().await?;
                    targets.remove(&loser);
                    undone += 1;
                }
                Err(e) => {
                    sp.rollback().await?;
                    tracing::warn!(entity = %loser, error = %e, "could not undo a merge");
                    break;
                }
            }
        }
    }
    Ok(undone)
}

/// Before a hard delete: keep the artifacts linked through this one (its
/// derivations and versions) in one source family, since the cascade would
/// otherwise drop every edge that joined them.
async fn relink_around(conn: &mut PgConnection, artifact: Uuid) -> Result<(), ApiError> {
    let parents: Vec<Uuid> = sqlx::query_scalar(
        "SELECT parent_artifact_id FROM artifact_derivations WHERE child_artifact_id = $1 \
         UNION SELECT supersedes_artifact_id FROM artifacts \
               WHERE id = $1 AND supersedes_artifact_id IS NOT NULL \
         ORDER BY 1",
    )
    .bind(artifact)
    .fetch_all(&mut *conn)
    .await?;
    let children: Vec<Uuid> = sqlx::query_scalar(
        "SELECT child_artifact_id FROM artifact_derivations WHERE parent_artifact_id = $1 \
         UNION SELECT id FROM artifacts WHERE supersedes_artifact_id = $1 \
         ORDER BY 1",
    )
    .bind(artifact)
    .fetch_all(&mut *conn)
    .await?;
    let Some(anchor) = parents.first().or(children.first()).copied() else {
        return Ok(());
    };
    for other in parents.iter().chain(&children).filter(|&&x| x != anchor) {
        sqlx::query(
            "INSERT INTO artifact_derivations (child_artifact_id, parent_artifact_id, kind, actor) \
             VALUES ($1, $2, 'other', 'safety') ON CONFLICT DO NOTHING",
        )
        .bind(other)
        .bind(anchor)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Undo automatic supersessions where one of `units` was the newer claim:
/// the older claim becomes current again.
async fn revert_supersessions(conn: &mut PgConnection, units: &[Uuid]) -> Result<u64, ApiError> {
    let rows = sqlx::query(
        "SELECT id, subject_ids, scope FROM inference_certificates \
         WHERE conclusion_kind = 'fact_supersession' AND decision = 'auto_applied' \
           AND superseded_at IS NULL AND retracted_at IS NULL AND subject_ids && $1",
    )
    .bind(units)
    .fetch_all(&mut *conn)
    .await?;
    let mut reverted = 0;
    for r in rows {
        let scope: Value = r.get("scope");
        if scope_uuid(&scope, "newer").is_some_and(|n| units.contains(&n)) {
            reverted += undo_supersession(conn, &scope).await?;
        }
    }
    Ok(reverted)
}

fn scope_uuid(scope: &Value, key: &str) -> Option<Uuid> {
    scope.get(key)?.as_str()?.parse().ok()
}

/// Make the older claim of one recorded supersession current again, with
/// the `valid_to` it had before. Returns 1 if it was still superseded.
pub(crate) async fn undo_supersession(
    conn: &mut PgConnection,
    scope: &Value,
) -> Result<u64, ApiError> {
    let (Some(older), Some(newer)) = (scope_uuid(scope, "older"), scope_uuid(scope, "newer"))
    else {
        return Ok(0);
    };
    let previous_valid_to = scope
        .get("previous_valid_to")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<chrono::DateTime<chrono::Utc>>().ok());
    let reverted = sqlx::query(
        "UPDATE atomic_units SET status = 'active', superseded_by_unit_id = NULL, \
           valid_to = $3, contradiction_scanned_at = NULL \
         WHERE id = $1 AND status = 'superseded' AND superseded_by_unit_id = $2",
    )
    .bind(older)
    .bind(newer)
    .bind(previous_valid_to)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE relationships SET status = 'active' \
         WHERE atomic_unit_id = $1 AND status = 'superseded'",
    )
    .bind(older)
    .execute(&mut *conn)
    .await?;
    Ok(reverted)
}

/// Withdraw open contradictions whose certificate was retracted, or that
/// involve a retracted unit.
async fn withdraw_contradictions(
    conn: &mut PgConnection,
    units: &[Uuid],
    certificates: &[Uuid],
    note: &str,
) -> Result<u64, ApiError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE contradictions SET status = 'dismissed', resolved_at = now(), \
           resolved_by = 'safety', resolution_note = $3 \
         WHERE status = 'open' \
           AND (unit_a_id = ANY($1) OR unit_b_id = ANY($1) OR certificate_id = ANY($2)) \
         RETURNING id",
    )
    .bind(units)
    .bind(certificates)
    .bind(note)
    .fetch_all(&mut *conn)
    .await?;
    for id in &ids {
        sqlx::query(
            "INSERT INTO contradiction_audit (contradiction_id, action, actor, from_status, \
               to_status, note) VALUES ($1, 'withdraw', 'safety', 'open', 'dismissed', $2)",
        )
        .bind(id)
        .bind(note)
        .execute(&mut *conn)
        .await?;
    }
    Ok(ids.len() as u64)
}

/// Everything that must follow when `units` stop being supported: their
/// graph edges, their open tray entries, supersessions they caused, open
/// contradictions about them, and every certificate resting on them.
pub async fn retract_unit_dependents(
    conn: &mut PgConnection,
    units: &[Uuid],
    reason: &str,
    event: Uuid,
    report: &mut RetractionReport,
) -> Result<(), ApiError> {
    if units.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE relationships SET status = 'retracted' \
         WHERE atomic_unit_id = ANY($1) AND status = 'active'",
    )
    .bind(units)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE review_queue SET state = 'dismissed' \
         WHERE target_kind = 'unit' AND target_id = ANY($1) AND state = 'open'",
    )
    .bind(units)
    .execute(&mut *conn)
    .await?;
    report.supersessions_reverted += revert_supersessions(conn, units).await?;
    let certs = propagate_withdrawal(conn, units, reason, event).await?;
    report.contradictions_withdrawn += withdraw_contradictions(
        conn,
        units,
        &certs,
        "withdrawn: a claim it rested on is no longer supported",
    )
    .await?;
    report.certificates_withdrawn.extend(certs);
    Ok(())
}

/// Retract a source artifact (and optionally delete it). Units whose only
/// support was this artifact are retracted with everything that rests on
/// them; units also supported elsewhere stay; photo groups are re-derived
/// without it. A certificate records the user's decision and heads the
/// causal chain of everything withdrawn.
pub async fn retract_artifact(
    pool: &PgPool,
    artifact: Uuid,
    reason: Option<String>,
    delete: bool,
    actor: Option<String>,
) -> Result<RetractionReport, ApiError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT original_filename, content_hash, kind::text AS kind FROM artifacts \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(artifact)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("artifact {artifact}")))?;
    let filename: Option<String> = row.get("original_filename");
    let reason_text = reason.unwrap_or_else(|| {
        if delete {
            "source deleted".to_string()
        } else {
            "source retracted".to_string()
        }
    });
    sqlx::query(
        "UPDATE artifacts SET retracted_at = coalesce(retracted_at, now()), \
           retraction_reason = $2 WHERE id = $1",
    )
    .bind(artifact)
    .bind(&reason_text)
    .execute(&mut *tx)
    .await?;

    let mut cert = user_decision(
        "user.retract_source",
        format!("source-retraction:{artifact}"),
        vec![artifact],
        EvidenceClass::Rejected,
        format!(
            "You {} \"{}\". Conclusions that relied on it were withdrawn.",
            if delete { "deleted" } else { "retracted" },
            filename.clone().unwrap_or_else(|| "a source".into())
        ),
    );
    cert.inputs[0].kind = "artifact".into();
    cert.inputs[0].detail = json!({
        "filename": filename,
        "content_hash": row.get::<String, _>("content_hash").trim(),
        "kind": row.get::<String, _>("kind"),
        "deleted": delete,
        "actor": actor.clone().unwrap_or_else(|| "local-user".into()),
    });
    cert.source_artifact_ids = vec![artifact];
    let event = store::record(&mut tx, &cert).await?;
    let mut report = RetractionReport {
        event_certificate: Some(event),
        deleted: delete,
        ..RetractionReport::default()
    };

    // Units with no remaining live source.
    let orphaned: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE atomic_units u SET status = 'retracted' \
         WHERE u.status <> 'retracted' \
           AND EXISTS (SELECT 1 FROM atomic_unit_provenance p \
                       WHERE p.atomic_unit_id = u.id AND p.artifact_id = $1) \
           AND NOT EXISTS (SELECT 1 FROM atomic_unit_provenance p \
                           JOIN artifacts a ON a.id = p.artifact_id \
                           WHERE p.atomic_unit_id = u.id AND a.retracted_at IS NULL) \
         RETURNING u.id",
    )
    .bind(artifact)
    .fetch_all(&mut *tx)
    .await?;
    report.units_retracted = orphaned.clone();
    let mut withdrawn = orphaned.clone();
    withdrawn.push(artifact);
    retract_unit_dependents(
        &mut tx,
        &orphaned,
        "a supporting source was removed",
        event,
        &mut report,
    )
    .await?;
    // Certificates resting on the artifact itself (photo groups, corroboration).
    let more = propagate_withdrawal(
        &mut tx,
        &withdrawn,
        "a supporting source was removed",
        event,
    )
    .await?;
    report.certificates_withdrawn.extend(more);
    report.certificates_withdrawn.sort();
    report.certificates_withdrawn.dedup();

    // Photos: leave their groups; the rest of each group is re-derived.
    let images: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM images WHERE artifact_id = $1")
        .bind(artifact)
        .fetch_all(&mut *tx)
        .await?;
    if !images.is_empty() {
        sqlx::query(
            "UPDATE images SET photo_grouped_at = NULL WHERE dup_cluster_id IN \
               (SELECT dup_cluster_id FROM images WHERE id = ANY($1) AND dup_cluster_id IS NOT NULL) \
             OR album_cluster_id IN \
               (SELECT album_cluster_id FROM images WHERE id = ANY($1) AND album_cluster_id IS NOT NULL)",
        )
        .bind(&images)
        .execute(&mut *tx)
        .await?;
        report.images_ungrouped = sqlx::query(
            "UPDATE images SET dup_cluster_id = NULL, album_cluster_id = NULL, \
               topic_cluster_id = NULL WHERE id = ANY($1)",
        )
        .bind(&images)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "DELETE FROM cluster_members WHERE member_kind = 'image' AND member_id = ANY($1)",
        )
        .bind(&images)
        .execute(&mut *tx)
        .await?;
    }

    // Automatic merges whose every source is gone are undone now rather than
    // at the next resolution pass.
    let merge_members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT unnest(subject_ids) FROM inference_certificates \
         WHERE id = ANY($1) AND conclusion_kind = 'entity_merge' AND decision = 'auto_applied'",
    )
    .bind(&report.certificates_withdrawn)
    .fetch_all(&mut *tx)
    .await?;
    if !merge_members.is_empty() {
        report.merges_withdrawn = unwind_auto_merges(
            &mut tx,
            &merge_members,
            "withdrawn: the only source behind this merge was removed",
        )
        .await?;
    }

    if delete {
        report.external_file_left =
            sqlx::query_scalar("SELECT storage_path FROM artifacts WHERE id = $1")
                .bind(artifact)
                .fetch_one(&mut *tx)
                .await?;
        relink_around(&mut tx, artifact).await?;
        sqlx::query("DELETE FROM artifacts WHERE id = $1")
            .bind(artifact)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(report)
}

/// A person says two photos are not copies of each other. Recorded as a
/// cannot-link no automatic grouping may override; any group that held both
/// is superseded and re-derived.
pub async fn mark_not_duplicate(
    pool: &PgPool,
    a: Uuid,
    b: Uuid,
    note: Option<String>,
) -> Result<Uuid, ApiError> {
    if a == b {
        return Err(ApiError::BadRequest("a photo is always itself".into()));
    }
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut tx = pool.begin().await?;
    let found: i64 = sqlx::query_scalar("SELECT count(*) FROM images WHERE id IN ($1, $2)")
        .bind(lo)
        .bind(hi)
        .fetch_one(&mut *tx)
        .await?;
    if found != 2 {
        return Err(ApiError::NotFound("image".into()));
    }
    sqlx::query(
        "INSERT INTO semantic_user_decisions (kind, a_id, b_id, note) \
         VALUES ('photo_not_duplicate', $1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(lo)
    .bind(hi)
    .bind(&note)
    .execute(&mut *tx)
    .await?;
    let cert = user_decision(
        "user.photo_not_duplicate",
        format!("user-not-duplicate:{lo}:{hi}"),
        vec![lo, hi],
        EvidenceClass::Rejected,
        "You marked these photos as not duplicates; they won't be grouped automatically.".into(),
    );
    let event = store::record(&mut tx, &cert).await?;
    let groups: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM inference_certificates \
         WHERE conclusion_kind = 'photo_duplicate_group' AND decision = 'auto_applied' \
           AND subject_ids @> $1 AND superseded_at IS NULL AND retracted_at IS NULL",
    )
    .bind(vec![lo, hi])
    .fetch_all(&mut *tx)
    .await?;
    store::withdraw(
        &mut tx,
        &groups,
        Outcome::Superseded,
        "you marked two of these photos as not duplicates",
        Some(event),
    )
    .await?;
    sqlx::query("UPDATE images SET photo_grouped_at = NULL WHERE id IN ($1, $2)")
        .bind(lo)
        .bind(hi)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(event)
}

/// Record that `child` was derived from `parent`. Corroboration certificates
/// that counted the child as independent are withdrawn.
pub async fn add_derivation(
    pool: &PgPool,
    child: Uuid,
    parent: Uuid,
    kind: &str,
) -> Result<Vec<Uuid>, ApiError> {
    const KINDS: &[&str] = &[
        "copy",
        "summary",
        "export",
        "reingest",
        "version",
        "correction",
        "other",
    ];
    if !KINDS.contains(&kind) {
        return Err(ApiError::BadRequest(format!(
            "kind must be one of {}",
            KINDS.join(", ")
        )));
    }
    if child == parent {
        return Err(ApiError::BadRequest(
            "an artifact can't derive from itself".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let found: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE id IN ($1, $2)")
        .bind(child)
        .bind(parent)
        .fetch_one(&mut *tx)
        .await?;
    if found != 2 {
        return Err(ApiError::NotFound("artifact".into()));
    }
    sqlx::query(
        "INSERT INTO artifact_derivations (child_artifact_id, parent_artifact_id, kind) \
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(child)
    .bind(parent)
    .bind(kind)
    .execute(&mut *tx)
    .await?;
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM inference_certificates \
         WHERE rule_id = 'claim.corroboration' AND decision = 'auto_applied' \
           AND source_artifact_ids && $1 AND superseded_at IS NULL AND retracted_at IS NULL",
    )
    // Any corroboration resting on either side may have counted the two as
    // independent, whichever arrived first; support is recomputed live.
    .bind(vec![child, parent])
    .fetch_all(&mut *tx)
    .await?;
    let withdrawn = store::withdraw(
        &mut tx,
        &stale,
        Outcome::Superseded,
        "the source turned out to be derived from another one",
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(withdrawn)
}

/// Sources (with chunk fingerprints) supporting a unit, live artifacts only.
pub async fn unit_sources(conn: &mut PgConnection, unit: Uuid) -> Result<Vec<SourceRef>, ApiError> {
    let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT p.artifact_id, \
                encode(digest(coalesce(m.content, s.content, i.ocr_text, p.quote, ''), 'sha256'), 'hex') AS fp \
         FROM atomic_unit_provenance p \
         JOIN artifacts a ON a.id = p.artifact_id AND a.retracted_at IS NULL \
         LEFT JOIN messages m ON m.id = p.message_id \
         LEFT JOIN document_segments s ON s.id = p.document_segment_id \
         LEFT JOIN images i ON i.id = p.image_id \
         WHERE p.atomic_unit_id = $1",
    )
    .bind(unit)
    .fetch_all(&mut *conn)
    .await?;
    let mut refs: Vec<SourceRef> = rows
        .into_iter()
        .map(|(artifact, fingerprint)| SourceRef {
            artifact,
            fingerprint,
        })
        .collect();
    refs.sort();
    refs.dedup_by(|a, b| a.artifact == b.artifact);
    Ok(refs)
}

/// Declared derivations and version links reachable from `artifacts`.
pub async fn derivations_for(
    conn: &mut PgConnection,
    artifacts: &[Uuid],
) -> Result<Vec<Derivation>, ApiError> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        // Declared derivations and version links are one edge set, walked
        // together, so a version chain of any length stays one lineage.
        "WITH RECURSIVE edge(child, parent) AS ( \
             SELECT child_artifact_id, parent_artifact_id FROM artifact_derivations \
             UNION ALL \
             SELECT id, supersedes_artifact_id FROM artifacts \
             WHERE supersedes_artifact_id IS NOT NULL \
         ), \
         link(child, parent, depth) AS ( \
             SELECT child, parent, 1 FROM edge \
             WHERE child = ANY($1) OR parent = ANY($1) \
           UNION \
             SELECT e.child, e.parent, l.depth + 1 \
             FROM edge e JOIN link l \
               ON e.child IN (l.child, l.parent) OR e.parent IN (l.child, l.parent) \
             WHERE l.depth < 16 \
         ) SELECT DISTINCT child, parent FROM link",
    )
    .bind(artifacts)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(child, parent)| Derivation { child, parent })
        .collect())
}

/// How well a unit is supported: its sources, their families, and the
/// confidence that independent support (only) would justify.
pub async fn unit_support(pool: &PgPool, unit: Uuid) -> Result<Value, ApiError> {
    let mut conn = pool.acquire().await?;
    let base: Option<f32> = sqlx::query_scalar("SELECT confidence FROM atomic_units WHERE id = $1")
        .bind(unit)
        .fetch_optional(&mut *conn)
        .await?;
    let base = base.ok_or_else(|| ApiError::NotFound(format!("unit {unit}")))?;
    let sources = unit_sources(&mut conn, unit).await?;
    let artifacts: Vec<Uuid> = sources.iter().map(|s| s.artifact).collect();
    let derivations = derivations_for(&mut conn, &artifacts).await?;
    let support = provenance::support(base, &sources, &derivations);
    let note = if support.artifacts > support.independent_sources {
        Some(super::ReasonCode::SourceNotIndependent.plain_language())
    } else {
        None
    };
    Ok(json!({
        "unit_id": unit,
        "support": support,
        "note": note,
    }))
}

/// Compare a new extractor version's reading of a unit's source with the
/// stored one. Agreement is recorded; disagreement is recorded and parked
/// for review, and the stored unit is left exactly as it was.
pub async fn apply_extraction_revision(
    pool: &PgPool,
    unit: Uuid,
    new_statement: &str,
    new_value: Option<String>,
    model_version: &str,
) -> Result<Value, ApiError> {
    use super::drift::{compare, ExtractedClaim};
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT u.statement, u.attrs->>'value' AS value, \
                coalesce(u.extraction_model, u.extraction_method::text) AS model, \
                e.name AS subject, \
                (SELECT p.artifact_id FROM atomic_unit_provenance p \
                 WHERE p.atomic_unit_id = u.id ORDER BY p.created_at LIMIT 1) AS artifact \
         FROM atomic_units u LEFT JOIN entities e ON e.id = u.subject_entity_id \
         WHERE u.id = $1",
    )
    .bind(unit)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("unit {unit}")))?;
    let anchor = format!("unit:{unit}");
    let subject: Option<String> = row.get("subject");
    let old = ExtractedClaim {
        id: unit,
        anchor: anchor.clone(),
        subject: subject.clone(),
        statement: row.get("statement"),
        value: row.get("value"),
        model_version: row.get::<Option<String>, _>("model").unwrap_or_default(),
    };
    let new = ExtractedClaim {
        id: Uuid::new_v5(&unit, format!("{model_version}:{new_statement}").as_bytes()),
        anchor,
        subject,
        statement: new_statement.to_string(),
        value: new_value,
        model_version: model_version.to_string(),
    };
    let report = compare(std::slice::from_ref(&old), std::slice::from_ref(&new));
    let artifact: Option<Uuid> = row.get("artifact");
    let mut out = Vec::new();
    for item in &report.items {
        let mut cert = item.certificate.clone();
        cert.source_artifact_ids = artifact.into_iter().collect();
        cert.conclusion_id = Some(unit);
        let id = store::record(&mut tx, &cert).await?;
        if item.high_impact {
            sqlx::query(
                "INSERT INTO review_queue (target_kind, target_id, reason, signals) \
                 VALUES ('unit', $1, 'model-disagreement', $2) ON CONFLICT DO NOTHING",
            )
            .bind(unit)
            .bind(json!({
                "certificate": id,
                "old_model": old.model_version,
                "new_model": model_version,
                "new_statement": new_statement,
                "fields": item.fields,
            }))
            .execute(&mut *tx)
            .await?;
        }
        out.push(json!({
            "certificate": id,
            "change": item.change,
            "fields": item.fields,
            "disagreement": item.high_impact,
        }));
    }
    tx.commit().await?;
    Ok(json!({"unit_id": unit, "items": out}))
}

/// Retracted artifact ids among `ids`, for callers filtering evidence.
pub async fn retracted_artifacts(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<BTreeSet<Uuid>, ApiError> {
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM artifacts WHERE id = ANY($1) AND retracted_at IS NOT NULL",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().collect())
}
