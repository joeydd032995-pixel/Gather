//! Perceptual hashing and near-duplicate grouping (pure, offline).
//!
//! The hash is the classic 64-bit DCT pHash: downscale to 32×32 luma, take the
//! 8×8 lowest-frequency DCT coefficients, and set a bit for each coefficient
//! above their median. Re-encoding, resizing and mild edits move only a few
//! bits, so near-duplicates sit within a small Hamming distance.

use std::collections::HashMap;

use image::{imageops, GrayImage};

use crate::cluster::UnionFind;

/// Side of the downscaled luma image the DCT runs over.
const SIZE: usize = 32;
/// Side of the low-frequency block the hash bits come from.
const LOW: usize = 8;
/// Hashing decodes at reduced scale: the 32×32 DCT input only needs a few
/// hundred source pixels per side (see `decode::decode_at_least`).
const DECODE_MIN_SIDE: u32 = 512;

/// Decode `bytes` (JPEG/PNG/WebP/TIFF/GIF/BMP) and hash it. `None` for
/// undecodable or oversized input (e.g. HEIC, which has no pure-Rust decoder):
/// such photos are simply left out of duplicate grouping.
pub fn compute_phash(bytes: &[u8]) -> Option<u64> {
    let img = super::decode::decode_at_least(bytes, DECODE_MIN_SIDE)?;
    Some(phash_luma(&img.to_luma8()))
}

/// pHash of an already-decoded luma image.
pub fn phash_luma(img: &GrayImage) -> u64 {
    // Area-filtered downscale: point sampling would alias fine texture into
    // the low frequencies and make re-encoded copies disagree.
    let small = imageops::resize(
        img,
        SIZE as u32,
        SIZE as u32,
        imageops::FilterType::Triangle,
    );
    let pixels: Vec<f64> = small.pixels().map(|p| f64::from(p[0])).collect();

    // cos((2x + 1) u π / 2N) for the LOW frequencies we keep.
    let cos: Vec<[f64; SIZE]> = (0..LOW)
        .map(|u| {
            let mut row = [0.0; SIZE];
            for (x, v) in row.iter_mut().enumerate() {
                *v = (((2 * x + 1) * u) as f64 * std::f64::consts::PI / (2 * SIZE) as f64).cos();
            }
            row
        })
        .collect();

    // Separable 2-D DCT-II, low block only: rows first, then columns.
    let mut rows = vec![[0.0f64; LOW]; SIZE];
    for (y, out) in rows.iter_mut().enumerate() {
        for (u, c) in cos.iter().enumerate() {
            out[u] = (0..SIZE).map(|x| pixels[y * SIZE + x] * c[x]).sum();
        }
    }
    let mut coeffs = [0.0f64; LOW * LOW];
    for v in 0..LOW {
        for u in 0..LOW {
            coeffs[v * LOW + u] = (0..SIZE).map(|y| rows[y][u] * cos[v][y]).sum();
        }
    }

    // Median over the AC terms: the DC term is overall brightness, which would
    // dominate the median without saying anything about structure.
    let mut ac: Vec<f64> = coeffs[1..].to_vec();
    ac.sort_by(f64::total_cmp);
    // 63 AC terms: odd count, so the median is the middle element.
    let median = ac[ac.len() / 2];

    coeffs.iter().enumerate().fold(
        0u64,
        |hash, (i, &c)| if c > median { hash | (1 << i) } else { hash },
    )
}

/// Number of differing bits.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Split 64 bits into `bands` contiguous chunks as evenly as possible.
fn band_ranges(bands: u32) -> Vec<(u32, u32)> {
    let bands = bands.clamp(1, 64);
    let base = 64 / bands;
    let extra = 64 % bands;
    let mut start = 0;
    (0..bands)
        .map(|i| {
            let width = base + u32::from(i < extra);
            let range = (start, width);
            start += width;
            range
        })
        .collect()
}

fn band_value(hash: u64, (start, width): (u32, u32)) -> u64 {
    if width == 64 {
        hash
    } else {
        (hash >> start) & ((1u64 << width) - 1)
    }
}

/// Group photos that are near-duplicates of each other: every photo in a group
/// is within `max_distance` bits of every other one.
///
/// Candidate pairs come from banding: split the hash into `max_distance + 1`
/// chunks; two hashes differing in at most `max_distance` bits must agree
/// exactly on at least one chunk (pigeonhole), so only photos sharing a bucket
/// are compared. Linked photos are first gathered transitively; a gathering
/// that is only a chain (A near B, B near C, A far from C) is then split into
/// groups whose members all match each other, so one in-between photo can't
/// tie two different shots together. Returns groups of 2+ indices, each
/// ascending, in order of their smallest member.
pub fn near_duplicate_groups(hashes: &[u64], max_distance: u32) -> Vec<Vec<usize>> {
    let mut uf = UnionFind::new(hashes.len());
    for band in band_ranges(max_distance + 1) {
        let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, &h) in hashes.iter().enumerate() {
            buckets.entry(band_value(h, band)).or_default().push(i);
        }
        for members in buckets.values() {
            for (pos, &i) in members.iter().enumerate() {
                for &j in &members[pos + 1..] {
                    if uf.find(i) != uf.find(j) && hamming(hashes[i], hashes[j]) <= max_distance {
                        uf.union(i, j);
                    }
                }
            }
        }
    }
    let comp: Vec<usize> = (0..hashes.len()).map(|i| uf.find(i)).collect();
    let near = |i: usize, j: usize| hamming(hashes[i], hashes[j]) <= max_distance;
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for component in crate::cluster::grouped(&comp) {
        if component.len() < 2 {
            continue;
        }
        let complete = component
            .iter()
            .enumerate()
            .all(|(p, &i)| component[p + 1..].iter().all(|&j| near(i, j)));
        if complete {
            groups.push(component);
            continue;
        }
        // Greedy and deterministic: each still-unplaced photo, in index order,
        // starts a group that takes every later photo near all its members.
        let mut placed = vec![false; component.len()];
        for start in 0..component.len() {
            if placed[start] {
                continue;
            }
            placed[start] = true;
            let mut group = vec![component[start]];
            for next in start + 1..component.len() {
                let j = component[next];
                if !placed[next] && group.iter().all(|&i| near(i, j)) {
                    placed[next] = true;
                    group.push(j);
                }
            }
            if group.len() >= 2 {
                groups.push(group);
            }
        }
    }
    groups.sort_by_key(|g| g[0]);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_cover_all_64_bits() {
        for bands in [1, 3, 7, 8, 17, 64] {
            let ranges = band_ranges(bands);
            assert_eq!(ranges.iter().map(|r| r.1).sum::<u32>(), 64);
            assert_eq!(ranges.len() as u32, bands);
        }
    }

    #[test]
    fn groups_mutual_near_duplicates_and_skips_far_hashes() {
        let a = 0xF0F0_F0F0_F0F0_F0F0u64;
        let b = a ^ 0b111; // 3 bits from a
        let c = a ^ (0b11 << 20); // 2 bits from a, 5 from b
        let far = !a;
        let groups = near_duplicate_groups(&[a, far, b, c], 5);
        assert_eq!(groups, vec![vec![0, 2, 3]]);
    }

    #[test]
    fn a_chain_through_one_photo_does_not_join_the_ends() {
        let a = 0xF0F0_F0F0_F0F0_F0F0u64;
        let b = a ^ 0b111; // 3 bits from a
        let c = b ^ (0b111 << 20); // 3 bits from b, 6 from a
                                   // b is near both, but a and c are too far apart to be copies of one
                                   // shot: a keeps b, and c is left on its own rather than chained in.
        let groups = near_duplicate_groups(&[a, b, c], 3);
        assert_eq!(groups, vec![vec![0, 1]]);
    }

    #[test]
    fn identical_hashes_group_and_distance_zero_is_exact() {
        let h = 0x1234_5678_9ABC_DEF0u64;
        assert_eq!(near_duplicate_groups(&[h, h, h ^ 1], 0), vec![vec![0, 1]]);
    }
}
