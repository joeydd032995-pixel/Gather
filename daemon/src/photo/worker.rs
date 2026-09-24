//! Photo worker (autonomous pipeline, Phase D).
//!
//! Interval-driven, same shape as the other workers. Each pass:
//!
//! 1. **Prepare** — claims unprepared images, computes the perceptual hash and
//!    EXIF GPS position off the async runtime, and stamps the cursor.
//! 2. **Regroup** — when newly prepared photos exist, re-derives near-duplicate
//!    groups and albums over the whole library and reconciles them with the
//!    existing clusters (a group keeps its cluster id when most of its members
//!    already had it). Tags are reversible columns; no photo is ever removed.
//! 3. **Caption** — only with `GATHER_OLLAMA_VISION_MODEL`: captions a batch
//!    with the local vision model, embeds the caption, and joins the photo to
//!    the topic of its nearest visual neighbour.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::albums::{album_label, segment_albums, Shot};
use super::phash::{compute_phash, hamming, near_duplicate_groups};
use crate::cluster::label_from_texts;
use crate::config::Config;
use crate::extract::image::analyze;
use crate::extract::ollama::OllamaClient;

#[derive(Debug, Default)]
pub struct PhotoStats {
    pub prepared: usize,
    pub duplicate_groups: usize,
    pub albums: usize,
    pub captioned: usize,
    pub topic_joins: usize,
}

/// Long-running entrypoint, spawned from main.
pub async fn worker_loop(pool: PgPool, config: Config) {
    let vision = vision_client(&config);
    let mut interval = tokio::time::interval(Duration::from_secs(config.photo_interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match run_one_pass(&pool, &config, vision.as_ref()).await {
            Ok(s) if s.prepared + s.captioned > 0 => tracing::info!(
                prepared = s.prepared,
                duplicate_groups = s.duplicate_groups,
                albums = s.albums,
                captioned = s.captioned,
                topic_joins = s.topic_joins,
                "photo pass complete"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "photo pass failed"),
        }
    }
}

/// The Ollama client, only when a vision model is configured.
pub fn vision_client(config: &Config) -> Option<OllamaClient> {
    match OllamaClient::from_config(config) {
        Ok(Some(client)) if client.vision_model.is_some() => Some(client),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "photo captions disabled");
            None
        }
    }
}

/// One pass. Public for tests.
pub async fn run_one_pass(
    pool: &PgPool,
    config: &Config,
    vision: Option<&OllamaClient>,
) -> anyhow::Result<PhotoStats> {
    let mut stats = PhotoStats {
        prepared: prepare_pass(pool, config).await?,
        ..PhotoStats::default()
    };
    regroup_pass(pool, config, &mut stats).await?;
    if let Some(client) = vision {
        caption_pass(pool, config, client, &mut stats).await?;
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// 1. Prepare: hash + GPS

async fn prepare_pass(pool: &PgPool, config: &Config) -> anyhow::Result<usize> {
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        "SELECT i.id, a.raw_content FROM images i \
         JOIN artifacts a ON a.id = i.artifact_id \
         WHERE i.photo_prepared_at IS NULL \
         ORDER BY i.id LIMIT $1 FOR UPDATE OF i SKIP LOCKED",
    )
    .bind(config.photo_batch)
    .fetch_all(&mut *tx)
    .await?;

    for row in &rows {
        let id: Uuid = row.get("id");
        let bytes: Option<Vec<u8>> = row.get("raw_content");
        // Decoding is CPU-bound; keep it off the async runtime. An artifact
        // stored only by path (no bytes) is stamped with no hash.
        let (phash, gps) = match bytes {
            Some(bytes) => {
                tokio::task::spawn_blocking(move || (compute_phash(&bytes), analyze(&bytes).gps))
                    .await?
            }
            None => (None, None),
        };
        sqlx::query(
            "UPDATE images SET phash = $2, latitude = $3, longitude = $4, \
             photo_prepared_at = now() WHERE id = $1",
        )
        .bind(id)
        // Stored as the same 64 bits in a signed column.
        .bind(phash.map(|h| h as i64))
        .bind(gps.map(|g| g.0))
        .bind(gps.map(|g| g.1))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(rows.len())
}

// ---------------------------------------------------------------------------
// 2. Regroup: near-duplicates + albums

/// A derived photo group, ready to reconcile with the stored clusters.
struct Group {
    members: Vec<Uuid>,
    label: String,
    cohesion: f32,
    representative: Uuid,
}

/// The two whole-library groupings, each tagging its own images column. SQL is
/// spelled out per grouping (static strings, no interpolation).
#[derive(Clone, Copy)]
enum Grouping {
    Duplicates,
    Albums,
}

impl Grouping {
    fn kind(self) -> &'static str {
        match self {
            Grouping::Duplicates => "photo_dup",
            Grouping::Albums => "album",
        }
    }
    fn current_sql(self) -> &'static str {
        match self {
            Grouping::Duplicates => {
                "SELECT id, dup_cluster_id FROM images WHERE dup_cluster_id IS NOT NULL"
            }
            Grouping::Albums => {
                "SELECT id, album_cluster_id FROM images WHERE album_cluster_id IS NOT NULL"
            }
        }
    }
    fn clear_sql(self) -> &'static str {
        match self {
            Grouping::Duplicates => {
                "UPDATE images SET dup_cluster_id = NULL \
                 WHERE dup_cluster_id IS NOT NULL AND NOT (id = ANY($1))"
            }
            Grouping::Albums => {
                "UPDATE images SET album_cluster_id = NULL \
                 WHERE album_cluster_id IS NOT NULL AND NOT (id = ANY($1))"
            }
        }
    }
    fn assign_sql(self) -> &'static str {
        match self {
            Grouping::Duplicates => {
                "UPDATE images i SET dup_cluster_id = v.cid \
                 FROM UNNEST($1::uuid[], $2::uuid[]) AS v(id, cid) \
                 WHERE i.id = v.id AND i.dup_cluster_id IS DISTINCT FROM v.cid"
            }
            Grouping::Albums => {
                "UPDATE images i SET album_cluster_id = v.cid \
                 FROM UNNEST($1::uuid[], $2::uuid[]) AS v(id, cid) \
                 WHERE i.id = v.id AND i.album_cluster_id IS DISTINCT FROM v.cid"
            }
        }
    }
    fn prune_sql(self) -> &'static str {
        match self {
            Grouping::Duplicates => {
                "DELETE FROM clusters c WHERE c.kind = 'photo_dup' \
                 AND NOT EXISTS (SELECT 1 FROM images i WHERE i.dup_cluster_id = c.id)"
            }
            Grouping::Albums => {
                "DELETE FROM clusters c WHERE c.kind = 'album' \
                 AND NOT EXISTS (SELECT 1 FROM images i WHERE i.album_cluster_id = c.id)"
            }
        }
    }
}

/// One prepared photo, as the regroup pass sees it.
struct PhotoRow {
    id: Uuid,
    phash: Option<u64>,
    pixels: i64,
    /// Capture time when known, else when the file was created/ingested.
    ordering_time: DateTime<Utc>,
    /// EXIF capture time only: albums need real capture times.
    taken_at: Option<DateTime<Utc>>,
    gps: Option<(f64, f64)>,
    filename: Option<String>,
}

async fn regroup_pass(
    pool: &PgPool,
    config: &Config,
    stats: &mut PhotoStats,
) -> anyhow::Result<()> {
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM images \
         WHERE photo_prepared_at IS NOT NULL AND photo_grouped_at IS NULL)",
    )
    .fetch_one(pool)
    .await?;
    if !pending {
        return Ok(());
    }

    let photos: Vec<PhotoRow> = sqlx::query(
        "SELECT i.id, i.phash, COALESCE(i.width, 0)::bigint * COALESCE(i.height, 0) AS pixels, \
                COALESCE(i.taken_at, a.source_created_at, a.ingested_at) AS ordering_time, \
                i.taken_at, i.latitude, i.longitude, a.original_filename \
         FROM images i JOIN artifacts a ON a.id = i.artifact_id \
         WHERE i.photo_prepared_at IS NOT NULL",
    )
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| {
        let lat: Option<f64> = r.get("latitude");
        let lon: Option<f64> = r.get("longitude");
        PhotoRow {
            id: r.get("id"),
            phash: r.get::<Option<i64>, _>("phash").map(|h| h as u64),
            pixels: r.get("pixels"),
            ordering_time: r.get("ordering_time"),
            taken_at: r.get("taken_at"),
            gps: lat.zip(lon),
            filename: r.get("original_filename"),
        }
    })
    .collect();

    let dup_groups = duplicate_groups(&photos, config.photo_dup_max_distance);
    stats.duplicate_groups = dup_groups.len();
    reconcile(pool, Grouping::Duplicates, &dup_groups).await?;

    let album_groups = albums(&photos, config);
    stats.albums = album_groups.len();
    reconcile(pool, Grouping::Albums, &album_groups).await?;

    // Stamp exactly the photos this pass saw, so one prepared concurrently
    // still triggers the next regroup.
    let seen: Vec<Uuid> = photos.iter().map(|p| p.id).collect();
    sqlx::query(
        "UPDATE images SET photo_grouped_at = now() \
         WHERE id = ANY($1) AND photo_grouped_at IS NULL",
    )
    .bind(&seen)
    .execute(pool)
    .await?;
    Ok(())
}

fn duplicate_groups(photos: &[PhotoRow], max_distance: u32) -> Vec<Group> {
    let hashed: Vec<&PhotoRow> = photos.iter().filter(|p| p.phash.is_some()).collect();
    let hashes: Vec<u64> = hashed.iter().filter_map(|p| p.phash).collect();
    near_duplicate_groups(&hashes, max_distance)
        .into_iter()
        .map(|idx| {
            let members: Vec<&PhotoRow> = idx.iter().map(|&i| hashed[i]).collect();
            // Sharpest copy wins; then the original (earliest); then id.
            let rep = *members
                .iter()
                .max_by(|a, b| {
                    a.pixels
                        .cmp(&b.pixels)
                        .then(b.ordering_time.cmp(&a.ordering_time))
                        .then(b.id.cmp(&a.id))
                })
                .expect("groups have 2+ members");
            let rep_hash = rep.phash.unwrap_or_default();
            let cohesion = members
                .iter()
                .map(|m| 1.0 - hamming(m.phash.unwrap_or_default(), rep_hash) as f32 / 64.0)
                .sum::<f32>()
                / members.len() as f32;
            Group {
                label: rep
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("{} near-duplicates", members.len())),
                members: members.iter().map(|m| m.id).collect(),
                cohesion,
                representative: rep.id,
            }
        })
        .collect()
}

fn albums(photos: &[PhotoRow], config: &Config) -> Vec<Group> {
    let dated: Vec<&PhotoRow> = photos.iter().filter(|p| p.taken_at.is_some()).collect();
    let shots: Vec<Shot> = dated
        .iter()
        .filter_map(|p| {
            p.taken_at.map(|taken_at| Shot {
                taken_at,
                gps: p.gps,
            })
        })
        .collect();
    segment_albums(
        &shots,
        chrono::Duration::hours(config.photo_album_gap_hours),
        config.photo_album_split_km,
        config.photo_album_min_size,
    )
    .into_iter()
    .map(|idx| {
        let first = &shots[idx[0]];
        let last = &shots[idx[idx.len() - 1]];
        Group {
            label: album_label(first.taken_at, last.taken_at),
            members: idx.iter().map(|&i| dated[i].id).collect(),
            cohesion: 1.0,
            representative: dated[idx[0]].id,
        }
    })
    .collect()
}

/// Make the stored clusters of one grouping match `groups`, reusing a cluster
/// id when most of a group's members already carry it, so ids (and anything a
/// UI bookmarked) stay stable as photos arrive. One transaction.
async fn reconcile(pool: &PgPool, grouping: Grouping, groups: &[Group]) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    let current: HashMap<Uuid, Uuid> = sqlx::query_as::<_, (Uuid, Uuid)>(grouping.current_sql())
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();

    let mut claimed: HashSet<Uuid> = HashSet::new();
    let mut image_ids: Vec<Uuid> = Vec::new();
    let mut cluster_ids: Vec<Uuid> = Vec::new();
    for group in groups {
        let mut votes: HashMap<Uuid, usize> = HashMap::new();
        for cluster in group.members.iter().filter_map(|m| current.get(m)) {
            if !claimed.contains(cluster) {
                *votes.entry(*cluster).or_default() += 1;
            }
        }
        let reuse = votes
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|(cluster, _)| cluster);

        let cluster_id = match reuse {
            Some(id) => {
                sqlx::query(
                    "UPDATE clusters SET label = $2, cohesion = $3, size = $4, \
                     representative_id = $5, updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(&group.label)
                .bind(group.cohesion)
                .bind(group.members.len() as i32)
                .bind(group.representative)
                .execute(&mut *tx)
                .await?;
                id
            }
            None => {
                sqlx::query_scalar(
                    "INSERT INTO clusters (kind, label, cohesion, size, representative_id) \
                     VALUES ($1, $2, $3, $4, $5) RETURNING id",
                )
                .bind(grouping.kind())
                .bind(&group.label)
                .bind(group.cohesion)
                .bind(group.members.len() as i32)
                .bind(group.representative)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        claimed.insert(cluster_id);

        sqlx::query("DELETE FROM cluster_members WHERE cluster_id = $1")
            .bind(cluster_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO cluster_members (cluster_id, member_kind, member_id, sim) \
             SELECT $1, 'image', m, $3 FROM UNNEST($2::uuid[]) AS m",
        )
        .bind(cluster_id)
        .bind(&group.members)
        .bind(group.cohesion)
        .execute(&mut *tx)
        .await?;

        image_ids.extend(&group.members);
        cluster_ids.extend(std::iter::repeat(cluster_id).take(group.members.len()));
    }

    sqlx::query(grouping.clear_sql())
        .bind(&image_ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query(grouping.assign_sql())
        .bind(&image_ids)
        .bind(&cluster_ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query(grouping.prune_sql()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Captions + visual topics (opt-in)

async fn caption_pass(
    pool: &PgPool,
    config: &Config,
    client: &OllamaClient,
    stats: &mut PhotoStats,
) -> anyhow::Result<()> {
    // Only decodable photos (hashed) are sent to the vision model.
    let rows = sqlx::query(
        "SELECT i.id, a.raw_content FROM images i \
         JOIN artifacts a ON a.id = i.artifact_id \
         WHERE i.captioned_at IS NULL AND i.phash IS NOT NULL AND a.raw_content IS NOT NULL \
         ORDER BY i.id LIMIT $1",
    )
    .bind(config.photo_batch)
    .fetch_all(pool)
    .await?;

    let model = client.vision_model.clone().unwrap_or_default();
    for row in &rows {
        let id: Uuid = row.get("id");
        let bytes: Vec<u8> = row.get("raw_content");
        let caption = match client.caption(&bytes).await {
            Ok(c) => c,
            Err(e) => {
                // Model down or overloaded: stop, retry the rest next pass.
                tracing::warn!(error = %e, "photo caption failed; retrying next pass");
                break;
            }
        };
        let embedding = client
            .embed(std::slice::from_ref(&caption))
            .await
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no embedding returned for caption"))?;

        sqlx::query(
            "UPDATE images SET caption = $2, caption_model = $3, embedding = $4, \
             captioned_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(&caption)
        .bind(format!("ollama:{model}"))
        .bind(Vector::from(embedding.clone()))
        .execute(pool)
        .await?;
        stats.captioned += 1;

        if join_nearest_topic(pool, config, id, &caption, embedding).await? {
            stats.topic_joins += 1;
        }
    }
    Ok(())
}

/// Attach a freshly captioned photo to its nearest visual neighbour's topic
/// (or start a topic with that neighbour) when they are similar enough.
async fn join_nearest_topic(
    pool: &PgPool,
    config: &Config,
    id: Uuid,
    caption: &str,
    embedding: Vec<f32>,
) -> anyhow::Result<bool> {
    let neighbour = sqlx::query(
        "SELECT id, topic_cluster_id, caption, (1 - (embedding <=> $1))::real AS sim \
         FROM images WHERE id <> $2 AND embedding IS NOT NULL \
         ORDER BY embedding <=> $1 LIMIT 1",
    )
    .bind(Vector::from(embedding))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(n) = neighbour else {
        return Ok(false);
    };
    let sim: f32 = n.get("sim");
    if !sim.is_finite() || sim < config.photo_topic_threshold {
        return Ok(false);
    }
    let neighbour_id: Uuid = n.get("id");
    let neighbour_topic: Option<Uuid> = n.get("topic_cluster_id");

    let mut tx = pool.begin().await?;
    let topic = match neighbour_topic {
        Some(topic) => {
            sqlx::query("UPDATE clusters SET size = size + 1, updated_at = now() WHERE id = $1")
                .bind(topic)
                .execute(&mut *tx)
                .await?;
            topic
        }
        None => {
            let neighbour_caption: String =
                n.get::<Option<String>, _>("caption").unwrap_or_default();
            let topic: Uuid = sqlx::query_scalar(
                "INSERT INTO clusters (kind, label, cohesion, size, representative_id) \
                 VALUES ('photo_topic', $1, $2, 2, $3) RETURNING id",
            )
            .bind(label_from_texts(&[caption, &neighbour_caption]))
            .bind(sim)
            .bind(neighbour_id)
            .fetch_one(&mut *tx)
            .await?;
            tag_topic(&mut tx, topic, neighbour_id, sim).await?;
            topic
        }
    };
    tag_topic(&mut tx, topic, id, sim).await?;
    tx.commit().await?;
    Ok(true)
}

async fn tag_topic(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    topic: Uuid,
    image: Uuid,
    sim: f32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO cluster_members (cluster_id, member_kind, member_id, sim) \
         VALUES ($1, 'image', $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(topic)
    .bind(image)
    .bind(sim)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE images SET topic_cluster_id = $2 WHERE id = $1")
        .bind(image)
        .bind(topic)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
