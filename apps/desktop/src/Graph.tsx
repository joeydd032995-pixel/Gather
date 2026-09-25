import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  type Simulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from "d3-force";
import { getGraphOverview, listUnits, type GraphOverview, type UnitSummary } from "./api";
import { KIND_LABELS } from "./Library";

interface Node extends SimulationNodeDatum {
  /** `e:<uuid>` for entities, `f:<uuid>` for files. */
  key: string;
  id: string;
  type: "entity" | "file";
  name: string;
  kind: string;
  weight: number;
  r: number;
}

interface Link extends SimulationLinkDatum<Node> {
  type: "relation" | "mention";
  label: string;
  count: number;
}

interface View {
  x: number;
  y: number;
  k: number;
}

const HEIGHT = 560;
const SIZES = [50, 150, 400];
const ENTITY_COLORS: Record<string, string> = {
  person: "#e0694f",
  organization: "#4a7dff",
  project: "#9b59d0",
  tool: "#1fa39a",
  concept: "#d4a017",
  location: "#3f9e4d",
  event: "#d65a9c",
  other: "#8a8f98",
};

function buildGraph(data: GraphOverview, showFiles: boolean) {
  const nodes: Node[] = data.entities.map((e) => ({
    key: `e:${e.id}`,
    id: e.id,
    type: "entity",
    name: e.name,
    kind: e.kind,
    weight: e.weight,
    r: 5 + Math.min(14, Math.sqrt(e.weight) * 2.2),
  }));
  const links: Link[] = data.relations.map((r) => ({
    source: `e:${r.source}`,
    target: `e:${r.target}`,
    type: "relation",
    label: r.relation_type.replace(/_/g, " "),
    count: r.count,
  }));
  if (showFiles) {
    for (const f of data.files) {
      nodes.push({
        key: `f:${f.id}`,
        id: f.id,
        type: "file",
        name: f.name,
        kind: f.kind,
        weight: f.mentions,
        r: 5 + Math.min(8, Math.sqrt(f.mentions) * 1.5),
      });
    }
    for (const m of data.mentions) {
      links.push({
        source: `f:${m.file_id}`,
        target: `e:${m.entity_id}`,
        type: "mention",
        label: "mentions",
        count: m.count,
      });
    }
  }
  return { nodes, links };
}

function endpoint(end: string | number | Node | undefined): Node | null {
  return end && typeof end === "object" ? end : null;
}

/** Names of the heaviest entities, which keep their labels when zoomed out. */
function labelledKeys(nodes: Node[]): Set<string> {
  return new Set(
    nodes
      .filter((n) => n.type === "entity")
      .sort((a, b) => b.weight - a.weight)
      .slice(0, 20)
      .map((n) => n.key),
  );
}

function EntityPanel({
  node,
  links,
  onSelect,
  onOpenFile,
}: {
  node: Node;
  links: Link[];
  onSelect: (key: string) => void;
  onOpenFile: (id: string) => void;
}) {
  const [units, setUnits] = useState<UnitSummary[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUnits(null);
    listUnits({ subjectEntityId: node.id, limit: 50 })
      .then((u) => !cancelled && setUnits(u))
      .catch(() => !cancelled && setUnits([]));
    return () => {
      cancelled = true;
    };
  }, [node.id]);

  const touching = links.filter(
    (l) => endpoint(l.source)?.key === node.key || endpoint(l.target)?.key === node.key,
  );
  const relations = touching.filter((l) => l.type === "relation");
  const files = touching
    .filter((l) => l.type === "mention")
    .map((l) => endpoint(l.source))
    .filter((n): n is Node => n !== null);

  return (
    <>
      <h3>{node.name}</h3>
      <p className="hint">
        {node.kind} · {node.weight} {node.weight === 1 ? "link" : "links"}
      </p>
      {relations.length > 0 && (
        <>
          <h4>Connections</h4>
          <ul className="graph-list">
            {relations.map((l, i) => {
              const source = endpoint(l.source)!;
              const target = endpoint(l.target)!;
              const outgoing = source.key === node.key;
              const other = outgoing ? target : source;
              return (
                <li key={i}>
                  {outgoing ? (
                    <>
                      {l.label} →{" "}
                      <button className="link-button" onClick={() => onSelect(other.key)}>
                        {other.name}
                      </button>
                    </>
                  ) : (
                    <>
                      <button className="link-button" onClick={() => onSelect(other.key)}>
                        {other.name}
                      </button>{" "}
                      {l.label} → this
                    </>
                  )}
                </li>
              );
            })}
          </ul>
        </>
      )}
      {files.length > 0 && (
        <>
          <h4>Mentioned in</h4>
          <ul className="graph-list">
            {files.map((f) => (
              <li key={f.key}>
                <button className="link-button" onClick={() => onOpenFile(f.id)}>
                  {f.name}
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
      <h4>What Gather knows about it</h4>
      {units === null ? (
        <p className="hint">Loading…</p>
      ) : units.length === 0 ? (
        <p className="hint">No statements are about this one directly; it appears in others'.</p>
      ) : (
        <ul className="lib-units">
          {units.map((u) => (
            <li key={u.id}>
              <span className={`lib-kind kind-${u.kind}`}>{u.kind}</span>
              <span>{u.statement}</span>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

function FilePanel({
  node,
  links,
  onSelect,
  onOpenFile,
}: {
  node: Node;
  links: Link[];
  onSelect: (key: string) => void;
  onOpenFile: (id: string) => void;
}) {
  const mentioned = links
    .filter((l) => l.type === "mention" && endpoint(l.source)?.key === node.key)
    .map((l) => endpoint(l.target))
    .filter((n): n is Node => n !== null);
  return (
    <>
      <h3>{node.name}</h3>
      <p className="hint">{KIND_LABELS[node.kind] ?? node.kind}</p>
      <button onClick={() => onOpenFile(node.id)}>Open in Library</button>
      <h4>Mentions</h4>
      <ul className="graph-list">
        {mentioned.map((e) => (
          <li key={e.key}>
            <button className="link-button" onClick={() => onSelect(e.key)}>
              {e.name}
            </button>
          </li>
        ))}
      </ul>
    </>
  );
}

interface GraphProps {
  onOpenFile: (id: string) => void;
}

export default function Graph({ onOpenFile }: GraphProps) {
  const [size, setSize] = useState(150);
  const [showFiles, setShowFiles] = useState(true);
  const [data, setData] = useState<GraphOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [view, setView] = useState<View>({ x: 0, y: 0, k: 1 });
  const [width, setWidth] = useState(800);
  const [selected, setSelected] = useState<string | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [, setFrame] = useState(0);

  const svgRef = useRef<SVGSVGElement>(null);
  const graph = useRef<{ nodes: Node[]; links: Link[] }>({ nodes: [], links: [] });
  const sim = useRef<Simulation<Node, Link> | null>(null);
  const viewRef = useRef(view);
  viewRef.current = view;
  // Until the user pans, zooms or drags, each finished layout is fitted to
  // the window.
  const interacted = useRef(false);
  const fitRef = useRef<() => void>(() => {});

  useEffect(() => {
    let cancelled = false;
    getGraphOverview(size, 100)
      .then((d) => {
        if (cancelled) return;
        setData(d);
        setError(null);
      })
      .catch((e: unknown) => !cancelled && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, [size]);

  // Track the drawing area's width so the graph fills it.
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    const observer = new ResizeObserver(([entry]) => setWidth(entry.contentRect.width));
    observer.observe(svg);
    return () => observer.disconnect();
  }, [data]);

  // Lay the graph out. The simulation cools quickly and then stops, so an
  // idle graph costs no CPU.
  useEffect(() => {
    if (!data) return;
    const built = buildGraph(data, showFiles);
    graph.current = built;
    let pending = false;
    const simulation = forceSimulation<Node, Link>(built.nodes)
      .force(
        "link",
        forceLink<Node, Link>(built.links)
          .id((n) => n.key)
          .distance((l) => (l.type === "mention" ? 70 : 55))
          .strength((l) => (l.type === "mention" ? 0.25 : 0.6)),
      )
      .force("charge", forceManyBody<Node>().strength(-140).distanceMax(400))
      .force("center", forceCenter(0, 0))
      .force("collide", forceCollide<Node>((n) => n.r + 3))
      .alphaDecay(0.045)
      .on("end", () => {
        if (!interacted.current) fitRef.current();
      })
      .on("tick", () => {
        if (pending) return;
        pending = true;
        requestAnimationFrame(() => {
          pending = false;
          setFrame((f) => f + 1);
        });
      });
    sim.current = simulation;
    return () => {
      simulation.stop();
    };
  }, [data, showFiles]);

  // Start centred.
  useEffect(() => {
    if (!interacted.current) setView((v) => ({ ...v, x: width / 2, y: HEIGHT / 2 }));
  }, [width]);

  const fit = useCallback(() => {
    const nodes = graph.current.nodes;
    if (nodes.length === 0) return;
    const xs = nodes.map((n) => n.x ?? 0);
    const ys = nodes.map((n) => n.y ?? 0);
    const [minX, maxX, minY, maxY] = [
      Math.min(...xs),
      Math.max(...xs),
      Math.min(...ys),
      Math.max(...ys),
    ];
    const k = Math.min(2, 0.9 * Math.min(width / (maxX - minX + 60), HEIGHT / (maxY - minY + 60)));
    setView({ k, x: width / 2 - k * ((minX + maxX) / 2), y: HEIGHT / 2 - k * ((minY + maxY) / 2) });
  }, [width]);
  fitRef.current = fit;

  // Wheel zoom around the cursor (a non-passive listener, so the page itself
  // doesn't scroll).
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      interacted.current = true;
      const rect = svg.getBoundingClientRect();
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      setView((v) => {
        const k = Math.min(4, Math.max(0.2, v.k * Math.exp(-e.deltaY * 0.0015)));
        return { k, x: mx - ((mx - v.x) / v.k) * k, y: my - ((my - v.y) / v.k) * k };
      });
    };
    svg.addEventListener("wheel", onWheel, { passive: false });
    return () => svg.removeEventListener("wheel", onWheel);
  }, [data]);

  // Dragging the background pans; dragging a node moves it; a click without
  // movement selects.
  const drag = useRef<{
    node: Node | null;
    startX: number;
    startY: number;
    view: View;
    moved: boolean;
  } | null>(null);

  const toGraph = (e: React.PointerEvent) => {
    const rect = svgRef.current!.getBoundingClientRect();
    const v = viewRef.current;
    return { x: (e.clientX - rect.left - v.x) / v.k, y: (e.clientY - rect.top - v.y) / v.k };
  };

  const onPointerDown = (e: React.PointerEvent, node: Node | null) => {
    e.stopPropagation();
    (e.target as Element).setPointerCapture(e.pointerId);
    drag.current = { node, startX: e.clientX, startY: e.clientY, view, moved: false };
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const d = drag.current;
    if (!d) return;
    if (Math.hypot(e.clientX - d.startX, e.clientY - d.startY) > 4) d.moved = true;
    if (!d.moved) return;
    interacted.current = true;
    if (d.node) {
      const p = toGraph(e);
      d.node.fx = p.x;
      d.node.fy = p.y;
      sim.current?.alphaTarget(0.2).restart();
    } else {
      setView({
        ...d.view,
        x: d.view.x + e.clientX - d.startX,
        y: d.view.y + e.clientY - d.startY,
      });
    }
  };

  const onPointerUp = () => {
    const d = drag.current;
    drag.current = null;
    if (!d) return;
    if (d.node) {
      d.node.fx = null;
      d.node.fy = null;
      sim.current?.alphaTarget(0);
    }
    if (!d.moved) setSelected(d.node ? d.node.key : null);
  };

  const { nodes, links } = graph.current;
  const byKey = useMemo(() => new Map(nodes.map((n) => [n.key, n])), [nodes]);
  const labelled = useMemo(() => labelledKeys(nodes), [nodes]);
  const focus = hovered ?? selected;
  const neighbours = useMemo(() => {
    const set = new Set<string>();
    if (!focus) return set;
    set.add(focus);
    for (const l of links) {
      const s = endpoint(l.source)?.key;
      const t = endpoint(l.target)?.key;
      if (s === focus && t) set.add(t);
      if (t === focus && s) set.add(s);
    }
    return set;
  }, [focus, links]);
  const needle = filter.trim().toLowerCase();
  const matches = useMemo(
    () => (needle ? new Set(nodes.filter((n) => n.name.toLowerCase().includes(needle)).map((n) => n.key)) : null),
    [needle, nodes],
  );

  const select = (key: string) => {
    setSelected(key);
    const n = byKey.get(key);
    if (n?.x !== undefined && n.y !== undefined) {
      setView((v) => ({ ...v, x: width / 2 - v.k * n.x!, y: HEIGHT / 2 - v.k * n.y! }));
    }
  };

  const dimmed = (key: string) =>
    (matches !== null && !matches.has(key)) || (focus !== null && !neighbours.has(key));

  if (error) return <p className="error">{error}</p>;
  if (!data) return <p className="hint">Loading…</p>;

  if (data.entities.length === 0) {
    return (
      <div className="graph-empty">
        <p>Your graph is empty for now.</p>
        <p className="hint">
          It grows as Gather finds people, places, tools and ideas in what you add, and how they
          connect. On its own, Gather links clear statements like "I work at Acme", "We decided to
          use Postgres" or "I prefer tea"; each becomes a line from you to that thing, and every
          file that mentions it connects to it too. With a local AI chat model (Ollama) it finds
          far more connections.
        </p>
      </div>
    );
  }

  const selectedNode = selected ? byKey.get(selected) : undefined;
  const showLabel = (n: Node) =>
    view.k >= 1.4 || neighbours.has(n.key) || (matches?.has(n.key) ?? false) || labelled.has(n.key);

  return (
    <div className="graph">
      <div className="graph-toolbar">
        <input
          type="search"
          placeholder="Find in graph…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && matches && matches.size > 0) select([...matches][0]);
          }}
        />
        <label>
          <input
            type="checkbox"
            checked={showFiles}
            onChange={(e) => setShowFiles(e.target.checked)}
          />{" "}
          Show files
        </label>
        <label>
          Show up to{" "}
          <select value={size} onChange={(e) => setSize(Number(e.target.value))}>
            {SIZES.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
        <button onClick={fit}>Fit</button>
      </div>
      {data.truncated && (
        <p className="hint">
          Showing the {data.entities.length} most connected of {data.entity_total}.
        </p>
      )}

      <div className="graph-body">
        <svg
          ref={svgRef}
          className="graph-canvas"
          height={HEIGHT}
          onPointerDown={(e) => onPointerDown(e, null)}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
        >
          <g transform={`translate(${view.x},${view.y}) scale(${view.k})`}>
            {links.map((l, i) => {
              const s = endpoint(l.source);
              const t = endpoint(l.target);
              if (!s || !t) return null;
              const faded = dimmed(s.key) || dimmed(t.key);
              return (
                <line
                  key={i}
                  className={`graph-link ${l.type}${faded ? " faded" : ""}`}
                  x1={s.x}
                  y1={s.y}
                  x2={t.x}
                  y2={t.y}
                  strokeWidth={Math.min(4, 1 + Math.log2(l.count)) / Math.sqrt(view.k)}
                >
                  <title>{`${s.name} ${l.label} ${t.name}`}</title>
                </line>
              );
            })}
            {nodes.map((n) => {
              const faded = dimmed(n.key);
              const cls = `graph-node ${n.type}${faded ? " faded" : ""}${n.key === selected ? " selected" : ""}`;
              return (
                <g
                  key={n.key}
                  className={cls}
                  transform={`translate(${n.x ?? 0},${n.y ?? 0})`}
                  onPointerDown={(e) => onPointerDown(e, n)}
                  onPointerEnter={() => setHovered(n.key)}
                  onPointerLeave={() => setHovered((h) => (h === n.key ? null : h))}
                >
                  {n.type === "entity" ? (
                    <circle r={n.r} fill={ENTITY_COLORS[n.kind] ?? ENTITY_COLORS.other} />
                  ) : (
                    <rect x={-n.r} y={-n.r} width={n.r * 2} height={n.r * 2} rx={2} />
                  )}
                  {showLabel(n) && (
                    <text y={n.r + 11} fontSize={11 / Math.sqrt(view.k)}>
                      {n.name.length > 28 ? `${n.name.slice(0, 27)}…` : n.name}
                    </text>
                  )}
                </g>
              );
            })}
          </g>
        </svg>

        <aside className="graph-panel">
          {selectedNode ? (
            selectedNode.type === "entity" ? (
              <EntityPanel
                node={selectedNode}
                links={links}
                onSelect={select}
                onOpenFile={onOpenFile}
              />
            ) : (
              <FilePanel
                node={selectedNode}
                links={links}
                onSelect={select}
                onOpenFile={onOpenFile}
              />
            )
          ) : (
            <>
              <p className="hint">
                Click a dot to see how it connects. Drag to move things around, scroll to zoom.
              </p>
              <ul className="graph-legend">
                {Object.entries(ENTITY_COLORS).map(([kind, color]) => (
                  <li key={kind}>
                    <svg width="12" height="12">
                      <circle cx="6" cy="6" r="5" fill={color} />
                    </svg>{" "}
                    {kind}
                  </li>
                ))}
                {showFiles && (
                  <li>
                    <svg width="12" height="12">
                      <rect className="graph-file-swatch" x="1" y="1" width="10" height="10" rx="2" />
                    </svg>{" "}
                    file
                  </li>
                )}
              </ul>
            </>
          )}
        </aside>
      </div>
    </div>
  );
}
