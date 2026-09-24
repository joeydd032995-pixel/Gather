//! EXIF time/place album segmentation (pure, offline).
//!
//! Photos sorted by capture time are cut into sessions: a new album starts when
//! the gap to the previous shot exceeds `gap`, or when both shots carry GPS and
//! are more than `split_km` apart (you drove to the next town). No reverse
//! geocoding: nothing leaves the machine.

use chrono::{DateTime, Duration, Utc};

/// One photo's capture metadata.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shot {
    pub taken_at: DateTime<Utc>,
    /// (latitude, longitude) in decimal degrees.
    pub gps: Option<(f64, f64)>,
}

/// Mean Earth radius, km.
const EARTH_RADIUS_KM: f64 = 6_371.0;

/// Great-circle distance between two (lat, lon) points, km.
pub fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

/// Segment shots into albums of at least `min_size` photos. Returns index
/// groups in capture order (ties broken by index, so the result is stable).
pub fn segment_albums(
    shots: &[Shot],
    gap: Duration,
    split_km: f64,
    min_size: usize,
) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..shots.len()).collect();
    order.sort_by(|&a, &b| shots[a].taken_at.cmp(&shots[b].taken_at).then(a.cmp(&b)));

    let mut albums: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    // Last known position in the current album: a GPS-less shot in between
    // must not hide a jump between two located ones.
    let mut last_gps: Option<(f64, f64)> = None;
    for i in order {
        let shot = shots[i];
        let breaks = current.last().is_some_and(|&prev| {
            let time_gap = shot.taken_at - shots[prev].taken_at > gap;
            let moved =
                matches!((last_gps, shot.gps), (Some(a), Some(b)) if haversine_km(a, b) > split_km);
            time_gap || moved
        });
        if breaks {
            albums.push(std::mem::take(&mut current));
            last_gps = None;
        }
        current.push(i);
        if shot.gps.is_some() {
            last_gps = shot.gps;
        }
    }
    albums.push(current);
    albums.retain(|a| a.len() >= min_size.max(1));
    albums
}

/// Human label for an album: its date, or date range when it spans days.
pub fn album_label(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let (s, e) = (start.date_naive(), end.date_naive());
    if s == e {
        s.to_string()
    } else {
        format!("{s} – {e}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: i64, gps: Option<(f64, f64)>) -> Shot {
        Shot {
            taken_at: Utc.with_ymd_and_hms(2026, 5, 3, 0, 0, 0).unwrap() + Duration::minutes(h),
            gps,
        }
    }

    #[test]
    fn haversine_matches_known_distance() {
        // Paris -> London ~ 344 km.
        let d = haversine_km((48.8566, 2.3522), (51.5074, -0.1278));
        assert!((d - 344.0).abs() < 5.0, "{d}");
    }

    #[test]
    fn labels_single_and_multi_day() {
        let d = Utc.with_ymd_and_hms(2026, 5, 3, 9, 0, 0).unwrap();
        assert_eq!(album_label(d, d), "2026-05-03");
        assert_eq!(
            album_label(d, d + Duration::days(2)),
            "2026-05-03 – 2026-05-05"
        );
    }

    #[test]
    fn a_gps_less_shot_does_not_hide_a_jump() {
        let paris = Some((48.8566, 2.3522));
        let lyon = Some((45.764, 4.8357));
        let shots = [at(0, paris), at(10, None), at(20, lyon), at(30, lyon)];
        let albums = segment_albums(&shots, Duration::hours(3), 10.0, 1);
        assert_eq!(albums, vec![vec![0, 1], vec![2, 3]]);
    }
}
