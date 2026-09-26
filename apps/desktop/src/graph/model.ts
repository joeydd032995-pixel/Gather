// The graph as the explorer draws it: nodes and links for d3-force, plus the
// communities that tint the background.

import type { SimulationLinkDatum, SimulationNodeDatum } from "d3-force";
import type { GraphOverview } from "../api";

export interface Node extends SimulationNodeDatum {
  /** `e:<uuid>` for entities, `f:<uuid>` for files. */
  key: string;
  id: string;
  type: "entity" | "file";
  name: string;
  kind: string;
  weight: number;
  r: number;
  /** Links touching this node, counted once the graph is built. */
  degree: number;
  /** Index into the communities list, or -1. */
  community: number;
}

export interface Link extends SimulationLinkDatum<Node> {
  type: "relation" | "mention";
  label: string;
  count: number;
}

export interface Graph {
  nodes: Node[];
  links: Link[];
  byKey: Map<string, Node>;
  /** Neighbour keys of each node. */
  adjacency: Map<string, Set<string>>;
  /** Groups of three or more tightly linked entities, largest first. */
  communities: Node[][];
}

/** Entity kinds with a colour of their own (tokens.css --cat-*). */
export const ENTITY_KINDS = [
  "person",
  "organization",
  "project",
  "tool",
  "concept",
  "location",
  "event",
  "other",
] as const;

export function kindKey(kind: string): string {
  return (ENTITY_KINDS as readonly string[]).includes(kind) ? kind : "other";
}

export function endpoint(end: string | number | Node | undefined): Node | null {
  return end && typeof end === "object" ? end : null;
}

interface BuildOptions {
  showFiles: boolean;
  /** Entity kinds the legend has switched off. */
  hiddenKinds: ReadonlySet<string>;
  /** Positions from the previous layout, so toggling a filter doesn't reshuffle everything. */
  previous?: Map<string, Node>;
}

export function buildGraph(data: GraphOverview, opts: BuildOptions): Graph {
  const nodes: Node[] = [];
  for (const e of data.entities) {
    if (opts.hiddenKinds.has(kindKey(e.kind))) continue;
    nodes.push({
      key: `e:${e.id}`,
      id: e.id,
      type: "entity",
      name: e.name,
      kind: e.kind,
      weight: e.weight,
      r: 6 + Math.min(16, Math.sqrt(e.weight) * 2.4),
      degree: 0,
      community: -1,
    });
  }
  if (opts.showFiles) {
    for (const f of data.files) {
      nodes.push({
        key: `f:${f.id}`,
        id: f.id,
        type: "file",
        name: f.name,
        kind: f.kind,
        weight: f.mentions,
        r: 6 + Math.min(6, Math.sqrt(f.mentions) * 1.4),
        degree: 0,
        community: -1,
      });
    }
  }

  const byKey = new Map(nodes.map((n) => [n.key, n]));
  for (const n of nodes) {
    const old = opts.previous?.get(n.key);
    if (old?.x !== undefined && old.y !== undefined) {
      n.x = old.x;
      n.y = old.y;
    }
  }

  const links: Link[] = [];
  const adjacency = new Map<string, Set<string>>(nodes.map((n) => [n.key, new Set()]));
  const connect = (a: string, b: string, link: Link) => {
    const na = byKey.get(a);
    const nb = byKey.get(b);
    if (!na || !nb || a === b) return;
    links.push(link);
    na.degree += 1;
    nb.degree += 1;
    adjacency.get(a)!.add(b);
    adjacency.get(b)!.add(a);
  };
  for (const r of data.relations) {
    const s = `e:${r.source}`;
    const t = `e:${r.target}`;
    connect(s, t, {
      source: s,
      target: t,
      type: "relation",
      label: r.relation_type.replace(/_/g, " "),
      count: r.count,
    });
  }
  if (opts.showFiles) {
    for (const m of data.mentions) {
      const s = `f:${m.file_id}`;
      const t = `e:${m.entity_id}`;
      connect(s, t, { source: s, target: t, type: "mention", label: "mentions", count: m.count });
    }
  }

  const communities = findCommunities(nodes, links);
  return { nodes, links, byKey, adjacency, communities };
}

/**
 * Label propagation over entity-to-entity relations: each entity repeatedly
 * takes the label most common among its neighbours. Cheap, deterministic
 * (fixed visiting order), and good enough to shade the obvious clusters.
 */
function findCommunities(nodes: Node[], links: Link[]): Node[][] {
  const entities = nodes.filter((n) => n.type === "entity");
  const label = new Map(entities.map((n, i) => [n.key, i]));
  const neighbours = new Map<string, string[]>(entities.map((n) => [n.key, []]));
  for (const l of links) {
    if (l.type !== "relation") continue;
    const s = typeof l.source === "string" ? l.source : endpoint(l.source)?.key;
    const t = typeof l.target === "string" ? l.target : endpoint(l.target)?.key;
    if (!s || !t) continue;
    neighbours.get(s)?.push(t);
    neighbours.get(t)?.push(s);
  }
  // Visit the most connected first, so hubs seed the communities.
  const order = [...entities].sort((a, b) => b.weight - a.weight || a.key.localeCompare(b.key));
  for (let round = 0; round < 12; round++) {
    let changed = false;
    for (const n of order) {
      const counts = new Map<number, number>();
      for (const m of neighbours.get(n.key) ?? []) {
        const l = label.get(m)!;
        // A hub's vote counts for little, so one very connected entity
        // (often "me") doesn't pull everything into a single group.
        counts.set(l, (counts.get(l) ?? 0) + 1 / Math.max(1, neighbours.get(m)!.length));
      }
      let best = label.get(n.key)!;
      let bestCount = counts.get(best) ?? 0;
      for (const [l, c] of counts) {
        if (c > bestCount || (c === bestCount && l < best)) {
          best = l;
          bestCount = c;
        }
      }
      if (best !== label.get(n.key)) {
        label.set(n.key, best);
        changed = true;
      }
    }
    if (!changed) break;
  }
  const groups = new Map<number, Node[]>();
  for (const n of entities) {
    const l = label.get(n.key)!;
    if (!groups.has(l)) groups.set(l, []);
    groups.get(l)!.push(n);
  }
  // A group holding most of the graph says nothing, so it isn't shaded.
  const communities = [...groups.values()]
    .filter((g) => g.length >= 3 && g.length <= Math.max(4, entities.length * 0.45))
    .sort((a, b) => b.length - a.length)
    .slice(0, 12);
  communities.forEach((g, i) => g.forEach((n) => (n.community = i)));
  return communities;
}

/** The most common entity kind in a community, which gives it its tint. */
export function dominantKind(members: Node[]): string {
  const counts = new Map<string, number>();
  for (const n of members)
    counts.set(kindKey(n.kind), (counts.get(kindKey(n.kind)) ?? 0) + n.weight);
  return [...counts.entries()].sort((a, b) => b[1] - a[1])[0]?.[0] ?? "other";
}
