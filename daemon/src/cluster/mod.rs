//! Graph clustering primitive (autonomous pipeline, Phase B).
//!
//! One reusable shape serves both jobs the write-up needs — entity resolution
//! (nodes = entities) and topic grouping (nodes = units): build a **mutual-kNN**
//! similarity graph, then take its connected components with union-find. The
//! edge weight is pgvector cosine when embeddings exist and the deterministic
//! text similarity (`entities::similarity` / `scan::score`) otherwise, so it
//! runs on a stock offline install.
//!
//! Why mutual-kNN rather than plain thresholding: single-linkage over a raw
//! similarity threshold chains (A~B, B~C, A≁C collapse into one blob). Requiring
//! each endpoint to be within the *other's* top-k, plus a size guard in the
//! caller, keeps components tight. Everything here is pure and deterministic —
//! no I/O — so the worker gathers inputs and acts on the output, and the whole
//! thing is unit-testable without a database (mirrors `scan::score`).

pub mod worker;

use uuid::Uuid;

/// An undirected similarity edge between two node indices, `a < b`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edge {
    pub a: usize,
    pub b: usize,
    pub sim: f32,
}

/// Build the mutual-kNN edge set. `sim(i, j)` must be symmetric and in [0,1].
/// An edge is kept only when `sim >= threshold` AND each node is within the
/// other's `k` nearest neighbours — the mutual condition that curbs chaining.
///
/// O(n²) similarity evaluations: fine at personal scale and for the bounded
/// per-pass batch the worker feeds it (the DB path pre-blocks candidates).
pub fn mutual_knn<F>(n: usize, k: usize, threshold: f32, sim: F) -> Vec<Edge>
where
    F: Fn(usize, usize) -> f32,
{
    if n < 2 || k == 0 {
        return Vec::new();
    }
    // Top-k neighbour set per node (indices only), by descending similarity.
    let neighbours: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let mut scored: Vec<(usize, f32)> = (0..n)
                .filter(|&j| j != i)
                .map(|j| (j, sim(i, j)))
                .filter(|&(_, s)| s >= threshold)
                .collect();
            // Descending sim; stable tie-break by index keeps it deterministic.
            scored.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
            // Keep the top k, but never split a group of equal-scoring
            // neighbours by index: cutting mid-tie makes the relation
            // asymmetric (a high-index node picks a low one but is never picked
            // back), stranding perfect matches as singletons. So retain every
            // neighbour whose score is >= the k-th best.
            if scored.len() > k {
                let cutoff = scored[k - 1].1;
                scored.retain(|&(_, s)| s >= cutoff);
            }
            scored.into_iter().map(|(j, _)| j).collect()
        })
        .collect();

    let mut edges = Vec::new();
    for i in 0..n {
        for &j in &neighbours[i] {
            if j > i && neighbours[j].contains(&i) {
                edges.push(Edge {
                    a: i,
                    b: j,
                    sim: sim(i, j),
                });
            }
        }
    }
    edges
}

/// Union-find (path compression + union by size). Shared with the photo
/// pipeline's near-duplicate grouping.
pub struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }
    pub fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    pub fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

/// Connected-component id per node (a representative index, not dense). Nodes
/// with no kept edge are their own singleton component.
pub fn components(n: usize, edges: &[Edge]) -> Vec<usize> {
    let mut uf = UnionFind::new(n);
    for e in edges {
        uf.union(e.a, e.b);
    }
    (0..n).map(|i| uf.find(i)).collect()
}

/// Group node indices by component id, each group sorted ascending. Groups are
/// returned in ascending order of their smallest member for determinism.
pub fn grouped(comp: &[usize]) -> Vec<Vec<usize>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (node, &c) in comp.iter().enumerate() {
        map.entry(c).or_default().push(node);
    }
    let mut groups: Vec<Vec<usize>> = map.into_values().collect();
    groups.sort_by_key(|g| g[0]);
    groups
}

/// Cohesion of a component: the mean similarity of its kept intra-component
/// edges, in [0,1]. A singleton has nothing to disagree with, so it scores 1.0.
/// Used to gate whether a cluster auto-labels or is parked for review.
pub fn cohesion(members: &[usize], edges: &[Edge]) -> f32 {
    if members.len() < 2 {
        return 1.0;
    }
    let set: std::collections::HashSet<usize> = members.iter().copied().collect();
    let intra: Vec<f32> = edges
        .iter()
        .filter(|e| set.contains(&e.a) && set.contains(&e.b))
        .map(|e| e.sim)
        .collect();
    if intra.is_empty() {
        return 0.0;
    }
    intra.iter().sum::<f32>() / intra.len() as f32
}

const LABEL_STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "our", "was", "were", "are",
    "you", "your", "his", "her", "its", "their", "not", "but", "all", "any", "can", "has", "have",
];

/// A short label for a cluster: the most frequent content token across its
/// members' texts (length >= 3, not a stopword). Deterministic tie-break by
/// first appearance. Empty string when there is nothing usable.
pub fn label_from_texts(texts: &[&str]) -> String {
    use std::collections::HashMap;
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new(); // token -> (count, first_seen)
    let mut order = 0usize;
    for text in texts {
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            let tok = raw.to_lowercase();
            if tok.chars().count() < 3 || LABEL_STOPWORDS.contains(&tok.as_str()) {
                continue;
            }
            let entry = counts.entry(tok).or_insert_with(|| {
                order += 1;
                (0, order)
            });
            entry.0 += 1;
        }
    }
    counts
        .into_iter()
        // Most frequent wins; ties broken by earliest first appearance.
        .max_by(|(_, (ca, oa)), (_, (cb, ob))| ca.cmp(cb).then(ob.cmp(oa)))
        .map(|(tok, _)| tok)
        .unwrap_or_default()
}

/// Ordering key for choosing a merge survivor: a specifically-typed entity
/// beats an extraction-created `other` (so a merge never discards the more
/// specific kind), then the longer name wins. Compare keys; higher survives.
pub fn survivor_key(kind: &str, name: &str) -> (u8, usize) {
    ((kind != "other") as u8, name.chars().count())
}

/// A stable id for an unordered entity pair, so a held merge review is keyed by
/// the pair (not one endpoint). Without this, two held suggestions sharing an
/// entity collide on `review_queue`'s (target_kind, target_id) unique index and
/// the second is silently dropped.
pub fn pair_key(a: Uuid, b: Uuid) -> Uuid {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("{lo}:{hi}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny fixed similarity matrix for deterministic graph tests.
    fn matrix_sim(m: &'static [&'static [f32]]) -> impl Fn(usize, usize) -> f32 {
        move |i, j| m[i][j]
    }

    #[test]
    fn duplicates_collapse_into_one_component() {
        // 0,1,2 mutually near-1; 3 is far from all.
        let m: &[&[f32]] = &[
            &[1.0, 0.95, 0.92, 0.10],
            &[0.95, 1.0, 0.93, 0.12],
            &[0.92, 0.93, 1.0, 0.08],
            &[0.10, 0.12, 0.08, 1.0],
        ];
        let edges = mutual_knn(4, 3, 0.6, matrix_sim(m));
        let groups = grouped(&components(4, &edges));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec![0, 1, 2]);
        assert_eq!(groups[1], vec![3]);
    }

    #[test]
    fn chaining_is_contained_by_the_threshold() {
        // A~B and B~C strong, A~C weak. With a 0.6 threshold A-C never edges,
        // but B bridges them — the mutual-kNN + union-find still yields ONE
        // component here (that is correct single-linkage behaviour); the caller
        // guards blobs by size. This test pins that A and C do NOT get a direct
        // edge, so the bridge is solely through B.
        let m: &[&[f32]] = &[&[1.0, 0.85, 0.30], &[0.85, 1.0, 0.82], &[0.30, 0.82, 1.0]];
        let edges = mutual_knn(3, 2, 0.6, matrix_sim(m));
        assert!(!edges.iter().any(|e| (e.a, e.b) == (0, 2)));
        assert!(edges.iter().any(|e| (e.a, e.b) == (0, 1)));
        assert!(edges.iter().any(|e| (e.a, e.b) == (1, 2)));
    }

    #[test]
    fn non_mutual_neighbour_does_not_edge() {
        // 0's nearest is 1, but 1's two nearest (k=1) is 2, not 0 -> no 0-1 edge.
        let m: &[&[f32]] = &[&[1.0, 0.70, 0.10], &[0.70, 1.0, 0.90], &[0.10, 0.90, 1.0]];
        let edges = mutual_knn(3, 1, 0.6, matrix_sim(m));
        assert_eq!(edges.len(), 1);
        assert_eq!((edges[0].a, edges[0].b), (1, 2));
    }

    #[test]
    fn cohesion_reflects_intra_edge_strength() {
        let edges = vec![
            Edge {
                a: 0,
                b: 1,
                sim: 0.9,
            },
            Edge {
                a: 1,
                b: 2,
                sim: 0.7,
            },
        ];
        assert!((cohesion(&[0, 1, 2], &edges) - 0.8).abs() < 1e-6);
        assert_eq!(cohesion(&[5], &edges), 1.0); // singleton
    }

    #[test]
    fn ties_at_the_kth_neighbour_are_not_split_by_index() {
        // Six identical nodes, k = 3. If truncation cut ties by index, nodes
        // 3-5 would pick 0-2 but never be picked back and strand as singletons.
        // Keeping all ties at the k-th score makes every pair mutual -> one
        // component of all six.
        let m: Vec<Vec<f32>> = (0..6)
            .map(|i| (0..6).map(|j| if i == j { 1.0 } else { 0.95 }).collect())
            .collect();
        let sim = |i: usize, j: usize| m[i][j];
        let edges = mutual_knn(6, 3, 0.6, sim);
        let groups = grouped(&components(6, &edges));
        assert_eq!(
            groups.len(),
            1,
            "identical nodes must not strand as singletons"
        );
        assert_eq!(groups[0], vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn label_picks_the_dominant_content_token() {
        let texts = [
            "I use Postgres daily",
            "Postgres and pgvector",
            "the postgres store",
        ];
        assert_eq!(label_from_texts(&texts), "postgres");
    }
}
