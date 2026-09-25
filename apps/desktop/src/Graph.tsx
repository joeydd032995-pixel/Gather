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
import {
  ArrowRight,
  FileText,
  Maximize2,
  Minus,
  MousePointerClick,
  Plus,
  Search,
  Waypoints,
} from "lucide-react";
import { getGraphOverview, type GraphOverview } from "./api";
import { kindLabel, plural } from "./kinds";
import {
  Button,
  Callout,
  EmptyState,
  IconButton,
  KindTag,
  PageHeader,
  Spinner,
  errorText,
} from "./ui";
import UnitList from "./UnitList";

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

const SIZES = [50, 150, 400];
/** Entity kinds with a colour of their own (tokens.css --cat-*). */
const ENTITY_KINDS = [
  "person",
  "organization",
  "project",
  "tool",
  "concept",
  "location",
  "event",
  "other",
];

function kindColor(kind: string): string {
  return `var(--cat-${ENTITY_KINDS.includes(kind) ? kind : "other"})`;
}

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
      <div className="panel-head">
        <KindTag kind={ENTITY_KINDS.includes(node.kind) ? node.kind : "other"} label={node.kind} />
        <h2 className="panel-title">{node.name}</h2>
        <p className="hint">{plural(node.weight, "link")}</p>
      </div>
      {relations.length > 0 && (
        <section className="panel-section">
          <h3 className="section-label">
            Connections <span className="count">{relations.length}</span>
          </h3>
          <ul className="relations">
            {relations.map((l, i) => {
              const source = endpoint(l.source)!;
              const target = endpoint(l.target)!;
              const outgoing = source.key === node.key;
              const other = outgoing ? target : source;
              return (
                <li key={i}>
                  <button type="button" className="relation" onClick={() => onSelect(other.key)}>
                    <span
                      className="relation-dot"
                      style={{ background: kindColor(other.kind) }}
                      aria-hidden
                    />
                    <span className="relation-text">
                      {outgoing ? (
                        <>
                          <span className="relation-verb">{l.label}</span> {other.name}
                        </>
                      ) : (
                        <>
                          {other.name} <span className="relation-verb">{l.label} this</span>
                        </>
                      )}
                    </span>
                    <ArrowRight className="relation-go" aria-hidden />
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      )}
      {files.length > 0 && (
        <section className="panel-section">
          <h3 className="section-label">
            Mentioned in <span className="count">{files.length}</span>
          </h3>
          <ul className="relations">
            {files.map((f) => (
              <li key={f.key}>
                <button type="button" className="relation" onClick={() => onOpenFile(f.id)}>
                  <FileText className="relation-icon" aria-hidden />
                  <span className="relation-text">{f.name}</span>
                  <ArrowRight className="relation-go" aria-hidden />
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
      <section className="panel-section">
        <h3 className="section-label">What Gather knows</h3>
        <UnitList
          subjectEntityId={node.id}
          pageSize={50}
          compact
          empty={
            <p className="hint">
              No statements are about this one directly; it appears in others'.
            </p>
          }
        />
      </section>
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
      <div className="panel-head">
        <KindTag kind="file" label={kindLabel(node.kind)} />
        <h2 className="panel-title">{node.name}</h2>
        <Button variant="subtle" size="sm" icon={ArrowRight} onClick={() => onOpenFile(node.id)}>
          Open in Library
        </Button>
      </div>
      <section className="panel-section">
        <h3 className="section-label">
          Mentions <span className="count">{mentioned.length}</span>
        </h3>
        <ul className="relations">
          {mentioned.map((e) => (
            <li key={e.key}>
              <button type="button" className="relation" onClick={() => onSelect(e.key)}>
                <span
                  className="relation-dot"
                  style={{ background: kindColor(e.kind) }}
                  aria-hidden
                />
                <span className="relation-text">{e.name}</span>
                <ArrowRight className="relation-go" aria-hidden />
              </button>
            </li>
          ))}
        </ul>
      </section>
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
  const [height, setHeight] = useState(560);
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
      .catch((e: unknown) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [size]);

  // Track the drawing area's size so the graph fills it.
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    const observer = new ResizeObserver(([entry]) => {
      setWidth(entry.contentRect.width);
      setHeight(entry.contentRect.height);
    });
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
      .force("charge", forceManyBody<Node>().strength(-170).distanceMax(420))
      .force("center", forceCenter(0, 0))
      // Room for an entity's label as well as its dot.
      .force(
        "collide",
        forceCollide<Node>((n) => n.r + (n.type === "entity" ? 12 : 4)),
      )
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
    if (!interacted.current) setView((v) => ({ ...v, x: width / 2, y: height / 2 }));
  }, [width, height]);

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
    const k = Math.min(2, 0.9 * Math.min(width / (maxX - minX + 60), height / (maxY - minY + 60)));
    setView({ k, x: width / 2 - k * ((minX + maxX) / 2), y: height / 2 - k * ((minY + maxY) / 2) });
  }, [width, height]);

  /** Zoom by `factor` around the centre of the canvas. */
  const zoomBy = (factor: number) => {
    interacted.current = true;
    setView((v) => {
      const k = Math.min(4, Math.max(0.2, v.k * factor));
      const cx = width / 2;
      const cy = height / 2;
      return { k, x: cx - ((cx - v.x) / v.k) * k, y: cy - ((cy - v.y) / v.k) * k };
    });
  };
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
    () =>
      needle
        ? new Set(nodes.filter((n) => n.name.toLowerCase().includes(needle)).map((n) => n.key))
        : null,
    [needle, nodes],
  );

  const select = (key: string) => {
    setSelected(key);
    const n = byKey.get(key);
    if (n?.x !== undefined && n.y !== undefined) {
      setView((v) => ({ ...v, x: width / 2 - v.k * n.x!, y: height / 2 - v.k * n.y! }));
    }
  };

  const dimmed = (key: string) =>
    (matches !== null && !matches.has(key)) || (focus !== null && !neighbours.has(key));

  const header = (
    <PageHeader
      title="Graph"
      description="The people, places, tools and ideas in your library, and the files that mention them."
      eyebrow={
        data && data.entities.length > 0
          ? data.truncated
            ? `${data.entities.length} most connected of ${data.entity_total.toLocaleString()}`
            : plural(data.entities.length, "entity", "entities")
          : undefined
      }
    />
  );

  if (error) {
    return (
      <>
        {header}
        <Callout title="Couldn't load the graph">{error}</Callout>
      </>
    );
  }
  if (!data) {
    return (
      <>
        {header}
        <div className="graph-loading">
          <Spinner label="Loading the graph" />
        </div>
      </>
    );
  }

  if (data.entities.length === 0) {
    return (
      <>
        {header}
        <EmptyState icon={Waypoints} title="Your graph is empty for now">
          <p>
            It grows as Gather finds people, places, tools and ideas in what you add, and how they
            connect. On its own, Gather links clear statements like “I work at Acme”, “We decided to
            use Postgres” or “I prefer tea”; each becomes a line from you to that thing, and every
            file that mentions it connects to it too.
          </p>
          <p>With a local AI chat model (Ollama) it finds far more connections.</p>
        </EmptyState>
      </>
    );
  }

  const selectedNode = selected ? byKey.get(selected) : undefined;
  const showLabel = (n: Node) =>
    view.k >= 1.4 || neighbours.has(n.key) || (matches?.has(n.key) ?? false) || labelled.has(n.key);
  const presentKinds = ENTITY_KINDS.filter((k) =>
    nodes.some(
      (n) =>
        n.type === "entity" && (n.kind === k || (k === "other" && !ENTITY_KINDS.includes(n.kind))),
    ),
  );

  return (
    <div className="graph">
      {header}
      <div className="graph-toolbar">
        <div className="search-field">
          <Search aria-hidden />
          <input
            className="input"
            type="search"
            aria-label="Find in graph"
            placeholder="Find in graph…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && matches && matches.size > 0) select([...matches][0]);
            }}
          />
          {matches && (
            <span className="search-hint num" aria-live="polite">
              {matches.size} found
            </span>
          )}
        </div>
        <label className="check">
          <input
            type="checkbox"
            checked={showFiles}
            onChange={(e) => setShowFiles(e.target.checked)}
          />
          <span>Show files</span>
        </label>
        <label className="inline-field">
          <span>Show up to</span>
          <select className="select" value={size} onChange={(e) => setSize(Number(e.target.value))}>
            {SIZES.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </select>
        </label>
      </div>

      <div className="graph-body">
        <div className="graph-stage">
          <svg
            ref={svgRef}
            className="graph-canvas"
            role="img"
            aria-label={`Graph of ${plural(nodes.length, "item")} and ${plural(links.length, "connection")}. Use Find in graph and press Enter to select one.`}
            onPointerDown={(e) => onPointerDown(e, null)}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
          >
            <defs>
              <pattern id="graph-dots" width="24" height="24" patternUnits="userSpaceOnUse">
                <circle cx="1" cy="1" r="1" className="graph-grid-dot" />
              </pattern>
            </defs>
            <rect width="100%" height="100%" fill="url(#graph-dots)" />
            <g transform={`translate(${view.x},${view.y}) scale(${view.k})`}>
              {links.map((l, i) => {
                const s = endpoint(l.source);
                const t = endpoint(l.target);
                if (!s || !t) return null;
                const faded = dimmed(s.key) || dimmed(t.key);
                const lit = focus !== null && !faded && (s.key === focus || t.key === focus);
                return (
                  <line
                    key={i}
                    className={`graph-link ${l.type}${faded ? " faded" : ""}${lit ? " lit" : ""}`}
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
                    {n.key === selected && (
                      <circle
                        className="graph-halo"
                        r={n.r + 7}
                        style={{
                          fill: n.type === "entity" ? kindColor(n.kind) : "var(--cat-file)",
                        }}
                      />
                    )}
                    {n.type === "entity" ? (
                      <circle r={n.r} style={{ fill: kindColor(n.kind) }} />
                    ) : (
                      <rect x={-n.r} y={-n.r} width={n.r * 2} height={n.r * 2} rx={2.5} />
                    )}
                    {showLabel(n) && (
                      <text y={n.r + 13} fontSize={11.5 / Math.sqrt(view.k)}>
                        {n.name.length > 28 ? `${n.name.slice(0, 27)}…` : n.name}
                      </text>
                    )}
                  </g>
                );
              })}
            </g>
          </svg>

          <div className="graph-controls" role="group" aria-label="Zoom">
            <IconButton icon={Plus} label="Zoom in" size="sm" onClick={() => zoomBy(1.3)} />
            <IconButton icon={Minus} label="Zoom out" size="sm" onClick={() => zoomBy(1 / 1.3)} />
            <span className="graph-controls-sep" aria-hidden />
            <IconButton icon={Maximize2} label="Fit to window" size="sm" onClick={fit} />
          </div>

          <ul className="graph-legend" aria-label="Legend">
            {presentKinds.map((kind) => (
              <li key={kind}>
                <KindTag kind={kind} />
              </li>
            ))}
            {showFiles && (
              <li>
                <span className="kind-tag">
                  <span className="legend-file" aria-hidden />
                  file
                </span>
              </li>
            )}
          </ul>
        </div>

        <aside className="graph-panel card" aria-label="Details" aria-live="polite">
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
            <div className="panel-intro">
              <span className="panel-intro-icon" aria-hidden>
                <MousePointerClick />
              </span>
              <h2 className="panel-title">Explore connections</h2>
              <p className="hint">
                Click a dot to see how it connects. Drag to move things around, scroll to zoom, or
                type a name above and press Enter.
              </p>
              <dl className="graph-stats">
                <div>
                  <dt>Entities</dt>
                  <dd className="num">{nodes.filter((n) => n.type === "entity").length}</dd>
                </div>
                <div>
                  <dt>Files</dt>
                  <dd className="num">{nodes.filter((n) => n.type === "file").length}</dd>
                </div>
                <div>
                  <dt>Links</dt>
                  <dd className="num">{links.length}</dd>
                </div>
              </dl>
            </div>
          )}
        </aside>
      </div>
    </div>
  );
}
