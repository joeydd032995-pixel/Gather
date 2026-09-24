//! Offline eval for the photo pipeline (Phase D). Deterministic, no DB:
//! synthetic scenes rendered in-process, re-encoded and resized the way real
//! copies drift, then hashed and grouped.

use std::io::Cursor;

use chrono::{Duration, TimeZone, Utc};
use image::codecs::jpeg::JpegEncoder;
use image::{imageops, ImageFormat, RgbImage};

use gather_daemon::extract::image::{dms_to_degrees, valid_position};
use gather_daemon::photo::albums::{segment_albums, Shot};
use gather_daemon::photo::phash::{compute_phash, hamming, near_duplicate_groups};

/// Default near-duplicate distance (GATHER_PHOTO_DUP_MAX_DISTANCE).
const MAX_DISTANCE: u32 = 6;

fn scene(kind: u8) -> RgbImage {
    RgbImage::from_fn(480, 360, |x, y| {
        let (xf, yf) = (x as f32, y as f32);
        let v = match kind {
            0 => 128.0 + 70.0 * (xf / 37.0).sin() * (yf / 23.0).cos() + xf / 8.0,
            1 => 128.0 + 100.0 * ((xf + yf) / 9.0).sin(),
            // Off-centre rings over a gradient. (A perfectly centred,
            // symmetric pattern is a pathological pHash input: symmetry zeroes
            // half the DCT terms, parking them on the median. Photos aren't.)
            _ => {
                let (dx, dy) = (xf - 170.0, yf - 140.0);
                100.0 + 90.0 * ((dx * dx + dy * dy).sqrt() / 18.0).cos() + yf / 5.0
            }
        };
        let l = v.clamp(0.0, 255.0) as u8;
        image::Rgb([l, l.wrapping_add(40), 255 - l])
    })
}

fn jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(img)
        .unwrap();
    out
}

fn png(img: &RgbImage) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

/// The copies a real library accumulates: original, re-compressed, resized.
fn copies(img: &RgbImage) -> Vec<Vec<u8>> {
    let half = imageops::resize(img, 240, 180, imageops::FilterType::Triangle);
    vec![png(img), jpeg(img, 60), jpeg(&half, 85)]
}

#[test]
fn copies_of_a_scene_hash_close_and_different_scenes_hash_far() {
    let hashes: Vec<Vec<u64>> = (0..3)
        .map(|k| {
            copies(&scene(k))
                .iter()
                .map(|b| compute_phash(b).expect("decodable"))
                .collect()
        })
        .collect();
    for scene_hashes in &hashes {
        for h in scene_hashes {
            assert!(
                hamming(*h, scene_hashes[0]) <= MAX_DISTANCE,
                "copy drifted {} bits",
                hamming(*h, scene_hashes[0])
            );
        }
    }
    for a in 0..3 {
        for b in (a + 1)..3 {
            let d = hamming(hashes[a][0], hashes[b][0]);
            assert!(
                d > 2 * MAX_DISTANCE,
                "scenes {a} and {b} only {d} bits apart"
            );
        }
    }
}

#[test]
fn a_mixed_library_groups_exactly_by_scene() {
    // Interleave the copies of three scenes, as an import would.
    let mut hashes = Vec::new();
    let mut truth = Vec::new();
    for copy in 0..3 {
        for k in 0..3u8 {
            hashes.push(compute_phash(&copies(&scene(k))[copy]).unwrap());
            truth.push(k);
        }
    }
    let groups = near_duplicate_groups(&hashes, MAX_DISTANCE);
    assert_eq!(groups.len(), 3);
    for g in &groups {
        assert_eq!(g.len(), 3);
        assert!(
            g.iter().all(|&i| truth[i] == truth[g[0]]),
            "mixed group {g:?}"
        );
    }
}

#[test]
fn undecodable_input_is_skipped_not_fatal() {
    assert_eq!(compute_phash(b"definitely not an image"), None);
    assert_eq!(compute_phash(&[]), None);
}

#[test]
fn albums_split_on_time_gaps_and_travel() {
    let day = Utc.with_ymd_and_hms(2026, 5, 3, 9, 0, 0).unwrap();
    let paris = Some((48.8566, 2.3522));
    let lyon = Some((45.764, 4.8357));
    let shot = |minutes: i64, gps| Shot {
        taken_at: day + Duration::minutes(minutes),
        gps,
    };
    let shots = vec![
        // Morning walk in Paris.
        shot(0, paris),
        shot(20, paris),
        shot(45, None),
        // Evening, same city, after a 6-hour break: new album.
        shot(420, paris),
        shot(430, paris),
        // Drove to Lyon within the hour: new album on distance alone.
        shot(470, lyon),
        shot(480, lyon),
        // A lone shot the next week: below the minimum size.
        shot(60 * 24 * 7, None),
    ];
    let albums = segment_albums(&shots, Duration::hours(3), 10.0, 2);
    assert_eq!(albums, vec![vec![0, 1, 2], vec![3, 4], vec![5, 6]]);
}

#[test]
fn gps_conversion_and_validation() {
    let lat = dms_to_degrees(48.0, 51.0, 23.76, false);
    assert!((lat - 48.8566).abs() < 1e-4);
    assert!(dms_to_degrees(0.0, 7.0, 40.0, true) < 0.0);
    assert_eq!(valid_position(0.0, 0.0), None, "no-fix sentinel");
    assert_eq!(valid_position(91.0, 0.0), None);
    assert_eq!(valid_position(f64::NAN, 1.0), None);
    assert_eq!(valid_position(48.85, 2.35), Some((48.85, 2.35)));
}
