// Canvas drawing for the graph explorer. Everything is drawn from design
// tokens read off the document, so both themes stay in step with the CSS.

import { dominantKind, endpoint, kindKey, type Graph, type Link, type Node } from "./model";

export interface View {
  x: number;
  y: number;
  k: number;
}

interface KindInk {
  base: string;
  light: string;
}

export interface Palette {
  dark: boolean;
  bg: string;
  spot: string;
  grid: string;
  text: string;
  text2: string;
  text3: string;
  surface: string;
  border: string;
  accent: string;
  file: string;
  fileFold: string;
  kinds: Record<string, KindInk>;
  font: string;
}

function parseColor(color: string): [number, number, number, number] {
  const c = color.trim();
  if (c.startsWith("#")) {
    const hex = c.length === 4 ? [...c.slice(1)].map((x) => x + x).join("") : c.slice(1, 7);
    return [
      parseInt(hex.slice(0, 2), 16),
      parseInt(hex.slice(2, 4), 16),
      parseInt(hex.slice(4, 6), 16),
      1,
    ];
  }
  const m = c.match(/rgba?\(([^)]+)\)/);
  if (m) {
    const [r, g, b, a = "1"] = m[1].split(",").map((s) => s.trim());
    return [Number(r), Number(g), Number(b), Number(a)];
  }
  return [128, 128, 128, 1];
}

/** `color` at opacity `a`. */
export function alpha(color: string, a: number): string {
  const [r, g, b, base] = parseColor(color);
  return `rgba(${r},${g},${b},${(a * base).toFixed(3)})`;
}

function mix(color: string, other: string, t: number): string {
  const [r1, g1, b1] = parseColor(color);
  const [r2, g2, b2] = parseColor(other);
  const m = (x: number, y: number) => Math.round(x + (y - x) * t);
  return `rgb(${m(r1, r2)},${m(g1, g2)},${m(b1, b2)})`;
}

/** The current theme's colours, as the canvas needs them. */
export function readPalette(): Palette {
  const css = getComputedStyle(document.documentElement);
  const v = (name: string) => css.getPropertyValue(name).trim() || "#888888";
  const theme = document.documentElement.dataset.theme;
  const dark =
    theme === "dark" || (theme !== "light" && matchMedia("(prefers-color-scheme: dark)").matches);
  const kinds: Record<string, KindInk> = {};
  for (const k of [
    "person",
    "organization",
    "project",
    "tool",
    "concept",
    "location",
    "event",
    "other",
  ]) {
    const base = v(`--cat-${k}`);
    kinds[k] = { base, light: mix(base, "#ffffff", dark ? 0.35 : 0.45) };
  }
  return {
    dark,
    bg: v("--surface-2"),
    spot: v("--accent"),
    grid: v("--border-strong"),
    text: v("--text"),
    text2: v("--text-2"),
    text3: v("--text-3"),
    surface: v("--surface"),
    border: v("--border"),
    accent: v("--accent"),
    file: v("--cat-file"),
    fileFold: mix(v("--cat-file"), dark ? "#000000" : "#ffffff", 0.35),
    kinds,
    font: css.getPropertyValue("--font-ui").trim() || "system-ui, sans-serif",
  };
}

export interface Scene {
  graph: Graph;
  view: View;
  width: number;
  height: number;
  dpr: number;
  palette: Palette;
  /** Hovered, else selected: what the neighbourhood highlight follows. */
  focus: string | null;
  selected: string | null;
  hovered: string | null;
  /** Keys matching the search box, or null when it's empty. */
  matches: Set<string> | null;
  /** 0→1 over the half-second after a selection, for its ripple. */
  pulse: number;
  /** Top entities by weight, which keep their labels when zoomed out. */
  anchors: Set<string>;
  /** Width not covered by the detail drawer. */
  visibleWidth: number;
}

/** Control point of the gentle curve every link is drawn with. */
function curve(s: Node, t: Node): [number, number] {
  const sx = s.x ?? 0;
  const sy = s.y ?? 0;
  const tx = t.x ?? 0;
  const ty = t.y ?? 0;
  const mx = (sx + tx) / 2;
  const my = (sy + ty) / 2;
  return [mx + (ty - sy) * 0.12, my - (tx - sx) * 0.12];
}

function isDimmed(scene: Scene, key: string): boolean {
  if (scene.matches && !scene.matches.has(key)) return true;
  if (scene.focus && key !== scene.focus && !scene.graph.adjacency.get(scene.focus)?.has(key)) {
    return true;
  }
  return false;
}

function drawBackdrop(ctx: CanvasRenderingContext2D, scene: Scene) {
  const { width: w, height: h, palette: p, view } = scene;
  ctx.fillStyle = p.bg;
  ctx.fillRect(0, 0, w, h);
  const spot = ctx.createRadialGradient(
    w * 0.5,
    h * 0.45,
    0,
    w * 0.5,
    h * 0.45,
    Math.max(w, h) * 0.7,
  );
  spot.addColorStop(0, alpha(p.spot, p.dark ? 0.09 : 0.06));
  spot.addColorStop(1, alpha(p.spot, 0));
  ctx.fillStyle = spot;
  ctx.fillRect(0, 0, w, h);

  // A dot grid fixed to the graph, so panning and zooming feel physical.
  let step = 32 * view.k;
  while (step < 18) step *= 2;
  while (step > 64) step /= 2;
  const ox = ((view.x % step) + step) % step;
  const oy = ((view.y % step) + step) % step;
  ctx.fillStyle = alpha(p.grid, p.dark ? 0.35 : 0.55);
  for (let x = ox; x < w; x += step) {
    for (let y = oy; y < h; y += step) {
      ctx.fillRect(x - 0.6, y - 0.6, 1.2, 1.2);
    }
  }
}

function drawCommunities(ctx: CanvasRenderingContext2D, scene: Scene) {
  const { graph, palette: p } = scene;
  const focusCommunity = scene.focus ? (graph.byKey.get(scene.focus)?.community ?? -1) : -1;
  // Each member casts a soft cloud; where a community is dense the clouds
  // pile up, so clusters read as regions without hard outlines.
  graph.communities.forEach((members, i) => {
    const color = p.kinds[dominantKind(members)].base;
    const quiet = scene.matches !== null || (scene.focus !== null && focusCommunity !== i);
    const strength = quiet ? 0.035 : p.dark ? 0.11 : 0.085;
    for (const n of members) {
      if (n.x === undefined || n.y === undefined) continue;
      const radius = 58 + n.r * 1.5;
      const g = ctx.createRadialGradient(n.x, n.y, 0, n.x, n.y, radius);
      g.addColorStop(0, alpha(color, strength));
      g.addColorStop(0.55, alpha(color, strength * 0.55));
      g.addColorStop(1, alpha(color, 0));
      ctx.fillStyle = g;
      ctx.beginPath();
      ctx.arc(n.x, n.y, radius, 0, Math.PI * 2);
      ctx.fill();
    }
  });
}

function nodeInk(p: Palette, n: Node): KindInk {
  return n.type === "file" ? { base: p.file, light: p.fileFold } : p.kinds[kindKey(n.kind)];
}

function drawLinks(ctx: CanvasRenderingContext2D, scene: Scene) {
  const { graph, palette: p, view } = scene;
  for (const l of graph.links) {
    const s = endpoint(l.source);
    const t = endpoint(l.target);
    if (!s || !t || s.x === undefined || t.x === undefined) continue;
    const touchesFocus = scene.focus !== null && (s.key === scene.focus || t.key === scene.focus);
    const faded = !touchesFocus && (isDimmed(scene, s.key) || isDimmed(scene, t.key));
    const [cx, cy] = curve(s, t);
    const base = Math.min(3, 1 + Math.log2(Math.max(1, l.count)) * 0.6);
    ctx.beginPath();
    ctx.moveTo(s.x, s.y!);
    ctx.quadraticCurveTo(cx, cy, t.x, t.y!);
    if (l.type === "mention") {
      ctx.setLineDash([2.5 / view.k, 4 / view.k]);
      ctx.strokeStyle = alpha(p.text3, faded ? 0.05 : touchesFocus ? 0.6 : 0.24);
      ctx.lineWidth = (touchesFocus ? 1.4 : 1) / view.k;
    } else {
      ctx.setLineDash([]);
      const a = faded ? 0.05 : touchesFocus ? 0.95 : p.dark ? 0.34 : 0.42;
      const g = ctx.createLinearGradient(s.x, s.y!, t.x, t.y!);
      g.addColorStop(0, alpha(nodeInk(p, s).base, a));
      g.addColorStop(1, alpha(nodeInk(p, t).base, a));
      ctx.strokeStyle = g;
      ctx.lineWidth = (touchesFocus ? base + 0.8 : base) / view.k;
    }
    ctx.stroke();
    ctx.setLineDash([]);

    // Direction, for the relations that touch what you're looking at.
    if (touchesFocus && l.type === "relation") {
      const tx = t.x;
      const ty = t.y!;
      const ang = Math.atan2(ty - cy, tx - cx);
      const back = t.r + 4 / view.k;
      const ax = tx - Math.cos(ang) * back;
      const ay = ty - Math.sin(ang) * back;
      const size = 7 / view.k;
      ctx.beginPath();
      ctx.moveTo(ax, ay);
      ctx.lineTo(ax - Math.cos(ang - 0.45) * size, ay - Math.sin(ang - 0.45) * size);
      ctx.lineTo(ax - Math.cos(ang + 0.45) * size, ay - Math.sin(ang + 0.45) * size);
      ctx.closePath();
      ctx.fillStyle = nodeInk(p, t).base;
      ctx.fill();
    }
  }
}

function roundRect(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.lineTo(x + w - r, y);
  ctx.arcTo(x + w, y, x + w, y + r, r);
  ctx.lineTo(x + w, y + h - r);
  ctx.arcTo(x + w, y + h, x + w - r, y + h, r);
  ctx.lineTo(x + r, y + h);
  ctx.arcTo(x, y + h, x, y + h - r, r);
  ctx.lineTo(x, y + r);
  ctx.arcTo(x, y, x + r, y, r);
  ctx.closePath();
}

function drawFile(ctx: CanvasRenderingContext2D, scene: Scene, n: Node) {
  const { palette: p, view } = scene;
  const w = n.r * 1.5;
  const h = n.r * 1.9;
  const x = n.x! - w / 2;
  const y = n.y! - h / 2;
  const fold = w * 0.36;
  ctx.beginPath();
  ctx.moveTo(x + 2, y);
  ctx.lineTo(x + w - fold, y);
  ctx.lineTo(x + w, y + fold);
  ctx.lineTo(x + w, y + h - 2);
  ctx.quadraticCurveTo(x + w, y + h, x + w - 2, y + h);
  ctx.lineTo(x + 2, y + h);
  ctx.quadraticCurveTo(x, y + h, x, y + h - 2);
  ctx.lineTo(x, y + 2);
  ctx.quadraticCurveTo(x, y, x + 2, y);
  ctx.closePath();
  ctx.fillStyle = p.file;
  ctx.fill();
  ctx.lineWidth = 1.5 / view.k;
  ctx.strokeStyle = p.bg;
  ctx.stroke();
  // The folded corner and two "lines of text".
  ctx.beginPath();
  ctx.moveTo(x + w - fold, y);
  ctx.lineTo(x + w - fold, y + fold);
  ctx.lineTo(x + w, y + fold);
  ctx.closePath();
  ctx.fillStyle = p.fileFold;
  ctx.fill();
  if (n.r * view.k > 7) {
    ctx.fillStyle = alpha(p.bg, 0.75);
    const lx = x + w * 0.2;
    ctx.fillRect(lx, y + h * 0.5, w * 0.6, Math.max(0.8, h * 0.07));
    ctx.fillRect(lx, y + h * 0.68, w * 0.42, Math.max(0.8, h * 0.07));
  }
}

function drawEntity(ctx: CanvasRenderingContext2D, scene: Scene, n: Node, glow: boolean) {
  const { palette: p, view, dpr } = scene;
  const ink = nodeInk(p, n);
  const x = n.x!;
  const y = n.y!;
  if (glow) {
    ctx.save();
    ctx.shadowColor = alpha(ink.base, p.dark ? 0.75 : 0.45);
    ctx.shadowBlur = Math.min(40, n.r * view.k * 1.4) * dpr;
  }
  const g = ctx.createRadialGradient(x - n.r * 0.35, y - n.r * 0.4, n.r * 0.1, x, y, n.r);
  g.addColorStop(0, ink.light);
  g.addColorStop(1, ink.base);
  ctx.beginPath();
  ctx.arc(x, y, n.r, 0, Math.PI * 2);
  ctx.fillStyle = g;
  ctx.fill();
  if (glow) ctx.restore();
  ctx.lineWidth = 2 / view.k;
  ctx.strokeStyle = p.bg;
  ctx.stroke();
}

function drawNodes(ctx: CanvasRenderingContext2D, scene: Scene) {
  const { graph, view, palette: p } = scene;
  const neighbours = scene.focus ? graph.adjacency.get(scene.focus) : undefined;
  // Files underneath, then entities from light to heavy; the focus on top.
  const order = [...graph.nodes].sort((a, b) => {
    const rank = (n: Node) =>
      (n.type === "entity" ? 1 : 0) +
      (neighbours?.has(n.key) ? 2 : 0) +
      (n.key === scene.focus ? 4 : 0);
    return rank(a) - rank(b) || a.weight - b.weight;
  });
  for (const n of order) {
    if (n.x === undefined || n.y === undefined) continue;
    const dim = isDimmed(scene, n.key);
    ctx.globalAlpha = dim ? (scene.matches ? 0.14 : 0.2) : 1;
    const glow =
      !dim &&
      n.type === "entity" &&
      (n.key === scene.focus ||
        (neighbours?.has(n.key) ?? false) ||
        (scene.focus === null && scene.anchors.has(n.key) && n.r > 12));
    if (n.type === "file") drawFile(ctx, scene, n);
    else drawEntity(ctx, scene, n, glow);
    ctx.globalAlpha = 1;

    if (n.key === scene.hovered && n.key !== scene.selected) {
      ctx.beginPath();
      ctx.arc(n.x, n.y, n.r + 4 / view.k, 0, Math.PI * 2);
      ctx.lineWidth = 1.5 / view.k;
      ctx.strokeStyle = alpha(p.text, 0.35);
      ctx.stroke();
    }
    if (n.key === scene.selected) {
      const color = nodeInk(p, n).base;
      ctx.beginPath();
      ctx.arc(n.x, n.y, n.r + 5 / view.k, 0, Math.PI * 2);
      ctx.lineWidth = 2.25 / view.k;
      ctx.strokeStyle = color;
      ctx.stroke();
      if (scene.pulse < 1) {
        const e = 1 - Math.pow(1 - scene.pulse, 3);
        ctx.beginPath();
        ctx.arc(n.x, n.y, n.r + (5 + 22 * e) / view.k, 0, Math.PI * 2);
        ctx.lineWidth = 2 / view.k;
        ctx.strokeStyle = alpha(color, 0.6 * (1 - e));
        ctx.stroke();
      }
    }
  }
}

interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

const overlaps = (a: Box, b: Box) =>
  a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

function pill(
  ctx: CanvasRenderingContext2D,
  p: Palette,
  box: Box,
  text: string,
  color: string,
  strong: boolean,
) {
  roundRect(ctx, box.x, box.y, box.w, box.h, box.h / 2);
  ctx.fillStyle = alpha(p.surface, p.dark ? 0.82 : 0.9);
  ctx.fill();
  ctx.lineWidth = 1;
  ctx.strokeStyle = strong ? alpha(p.text, 0.18) : alpha(p.border, 0.9);
  ctx.stroke();
  ctx.fillStyle = color;
  ctx.fillText(text, box.x + box.w / 2, box.y + box.h / 2 + 0.5);
}

/** Labels in screen space, most important first, never overlapping. */
function drawLabels(ctx: CanvasRenderingContext2D, scene: Scene) {
  const { graph, view, palette: p, width, height } = scene;
  const neighbours = scene.focus ? graph.adjacency.get(scene.focus) : undefined;
  const priority = (n: Node) => {
    if (n.key === scene.selected) return 0;
    if (n.key === scene.hovered) return 1;
    if (neighbours?.has(n.key)) return 2;
    if (scene.matches?.has(n.key)) return 3;
    if (scene.anchors.has(n.key)) return 4;
    return 5;
  };
  const candidates = graph.nodes
    .filter((n) => n.x !== undefined && !isDimmed(scene, n.key))
    .filter((n) => view.k >= 0.9 || priority(n) < 5)
    .sort((a, b) => priority(a) - priority(b) || b.weight - a.weight);

  const placed: Box[] = [];
  // Other nodes are obstacles too, so a label never hides a dot.
  const dots: (Box & { key: string })[] = graph.nodes
    .filter((n) => n.x !== undefined && !isDimmed(scene, n.key))
    .map((n) => {
      const r = n.r * view.k;
      return {
        key: n.key,
        x: n.x! * view.k + view.x - r,
        y: n.y! * view.k + view.y - r,
        w: 2 * r,
        h: 2 * r,
      };
    });
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";

  // Relation names along the focused node's links.
  if (scene.focus && view.k >= 0.7) {
    ctx.font = `600 10.5px ${p.font}`;
    for (const l of graph.links) {
      if (l.type !== "relation") continue;
      const s = endpoint(l.source);
      const t = endpoint(l.target);
      if (!s || !t || (s.key !== scene.focus && t.key !== scene.focus)) continue;
      const [cx, cy] = curve(s, t);
      // The curve's midpoint.
      const mx = 0.25 * s.x! + 0.5 * cx + 0.25 * t.x!;
      const my = 0.25 * s.y! + 0.5 * cy + 0.25 * t.y!;
      const sx = mx * view.k + view.x;
      const sy = my * view.k + view.y;
      const w = ctx.measureText(l.label).width + 12;
      const box = { x: sx - w / 2, y: sy - 9, w, h: 18 };
      if (placed.some((b) => overlaps(b, box))) continue;
      placed.push(box);
      pill(ctx, p, box, l.label, p.text3, false);
    }
  }

  for (const n of candidates) {
    // The hover card already names it.
    if (n.key === scene.hovered && n.key !== scene.selected) continue;
    const strong = priority(n) <= 2;
    ctx.font = `${strong ? 650 : 560} ${strong ? 12.5 : 11.5}px ${p.font}`;
    const text = n.name.length > 30 ? `${n.name.slice(0, 29)}…` : n.name;
    const w = ctx.measureText(text).width + 14;
    const h = strong ? 22 : 20;
    const cx = n.x! * view.k + view.x;
    const cy = n.y! * view.k + view.y;
    const reach = (n.type === "file" ? n.r * 0.95 : n.r) * view.k + 6;
    // Below the dot reads best; then above, right and left.
    const spots: Box[] = [
      { x: cx - w / 2, y: cy + reach, w, h },
      { x: cx - w / 2, y: cy - reach - h, w, h },
      { x: cx + reach, y: cy - h / 2, w, h },
      { x: cx - reach - w, y: cy - h / 2, w, h },
    ];
    const box = spots.find(
      (b) =>
        b.x >= 4 &&
        b.y >= 4 &&
        b.x + b.w <= width - 4 &&
        b.y + b.h <= height - 4 &&
        !placed.some((o) => overlaps(o, b)) &&
        (strong || !dots.some((d) => d.key !== n.key && overlaps(d, b))),
    );
    if (!box) continue;
    placed.push(box);
    pill(ctx, p, box, text, strong ? p.text : p.text2, strong);
  }
}

export function draw(ctx: CanvasRenderingContext2D, scene: Scene) {
  const { dpr, view } = scene;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  drawBackdrop(ctx, scene);
  ctx.setTransform(dpr * view.k, 0, 0, dpr * view.k, dpr * view.x, dpr * view.y);
  drawCommunities(ctx, scene);
  drawLinks(ctx, scene);
  drawNodes(ctx, scene);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  drawLabels(ctx, scene);
}

/** The node under a point in canvas pixels, preferring whatever is drawn on top. */
export function hitTest(graph: Graph, view: View, sx: number, sy: number): Node | null {
  const x = (sx - view.x) / view.k;
  const y = (sy - view.y) / view.k;
  const slack = 4 / view.k;
  let best: Node | null = null;
  let bestDist = Infinity;
  for (const n of graph.nodes) {
    if (n.x === undefined || n.y === undefined) continue;
    const d = Math.hypot(n.x - x, n.y - y);
    const reach = (n.type === "file" ? n.r * 1.05 : n.r) + slack;
    if (d <= reach && d - n.r < bestDist) {
      best = n;
      bestDist = d - n.r;
    }
  }
  return best;
}

export function bounds(nodes: Node[]) {
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const n of nodes) {
    if (n.x === undefined || n.y === undefined) continue;
    minX = Math.min(minX, n.x - n.r);
    maxX = Math.max(maxX, n.x + n.r);
    minY = Math.min(minY, n.y - n.r);
    maxY = Math.max(maxY, n.y + n.r);
  }
  if (!Number.isFinite(minX)) return null;
  return { minX, minY, maxX, maxY };
}

export interface MinimapFrame {
  scale: number;
  ox: number;
  oy: number;
}

/** The whole graph in miniature, with the visible area outlined. */
export function drawMinimap(
  ctx: CanvasRenderingContext2D,
  scene: Scene,
  w: number,
  h: number,
): MinimapFrame | null {
  const { graph, palette: p, dpr, view } = scene;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const b = bounds(graph.nodes);
  if (!b) return null;
  const pad = 10;
  const scale = Math.min(
    (w - pad * 2) / (b.maxX - b.minX || 1),
    (h - pad * 2) / (b.maxY - b.minY || 1),
  );
  const ox = pad + (w - pad * 2 - (b.maxX - b.minX) * scale) / 2 - b.minX * scale;
  const oy = pad + (h - pad * 2 - (b.maxY - b.minY) * scale) / 2 - b.minY * scale;
  for (const n of graph.nodes) {
    if (n.x === undefined) continue;
    ctx.beginPath();
    ctx.arc(n.x * scale + ox, n.y! * scale + oy, Math.max(1.2, n.r * scale * 0.9), 0, Math.PI * 2);
    ctx.fillStyle = alpha(nodeInk(p, n).base, isDimmed(scene, n.key) ? 0.3 : 0.9);
    ctx.fill();
  }
  // The viewport, in graph space, mapped into the minimap.
  const vx = (-view.x / view.k) * scale + ox;
  const vy = (-view.y / view.k) * scale + oy;
  const vw = (scene.visibleWidth / view.k) * scale;
  const vh = (scene.height / view.k) * scale;
  // Clamp to the map so a zoomed-out view still shows a sensible frame.
  const x0 = Math.max(1, vx);
  const y0 = Math.max(1, vy);
  const x1 = Math.min(w - 1, vx + vw);
  const y1 = Math.min(h - 1, vy + vh);
  if (x1 > x0 && y1 > y0) {
    ctx.fillStyle = alpha(p.accent, 0.08);
    ctx.fillRect(x0, y0, x1 - x0, y1 - y0);
    ctx.lineWidth = 1.25;
    ctx.strokeStyle = alpha(p.accent, 0.8);
    roundRect(ctx, x0, y0, x1 - x0, y1 - y0, 3);
    ctx.stroke();
  }
  return { scale, ox, oy };
}

export type { Graph, Link, Node };
