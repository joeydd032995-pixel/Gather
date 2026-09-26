import { useEffect, useRef, useState } from "react";
import {
  Maximize2,
  Combine,
  FileText,
  GitCompareArrows,
  House,
  Inbox,
  Plus,
  ScanText,
  ShieldCheck,
  Sparkles,
  Upload,
  Waypoints,
} from "lucide-react";
import { forceCenter, forceCollide, forceLink, forceManyBody, forceSimulation } from "d3-force";
import {
  getGraphOverview,
  listArtifacts,
  listContradictions,
  listMergeSuggestions,
  listReview,
  type ArtifactSummary,
  type ContradictionSummary,
  type GraphOverview,
  type MergeSuggestion,
  type ReviewItem,
} from "./api";
import { buildGraph, type Link, type Node } from "./graph/model";
import { bounds, draw, readPalette } from "./graph/render";
import { kindIcon, kindLabel, plural } from "./kinds";
import type { Tab } from "./nav";
import { Badge, Button, Panel, Skeleton, StatTile, Toolbar, When } from "./ui";

const FILE_CAP = 500;

function greeting(): string {
  const h = new Date().getHours();
  if (h < 5) return "Working late";
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

interface Snapshot {
  files: ArtifactSummary[];
  review: ReviewItem[];
  contradictions: ContradictionSummary[];
  merges: MergeSuggestion[];
  graph: GraphOverview | null;
}

/** A still, fitted rendering of the most connected part of the graph. */
function GraphPreview({ data, onOpen }: { data: GraphOverview; onOpen: () => void }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    const wrap = wrapRef.current;
    if (!canvas || !wrap) return;
    const graph = buildGraph(data, { showFiles: false, hiddenKinds: new Set() });
    // Lay it out once, synchronously: this is a picture, not the explorer.
    forceSimulation<Node, Link>(graph.nodes)
      .force(
        "link",
        forceLink<Node, Link>(graph.links)
          .id((n) => n.key)
          .distance(60)
          .strength(0.5),
      )
      .force("charge", forceManyBody<Node>().strength(-200))
      .force("center", forceCenter(0, 0))
      .force(
        "collide",
        forceCollide<Node>((n) => n.r + 12),
      )
      .stop()
      .tick(260);
    const anchors = new Set(
      [...graph.nodes]
        .sort((a, b) => b.weight - a.weight)
        .slice(0, 8)
        .map((n) => n.key),
    );

    const paint = () => {
      const { width, height } = wrap.getBoundingClientRect();
      if (width === 0 || height === 0) return;
      const dpr = Math.min(2, window.devicePixelRatio || 1);
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
      const b = bounds(graph.nodes);
      if (!b) return;
      const k = Math.min(
        1.6,
        0.88 * Math.min(width / (b.maxX - b.minX + 60), height / (b.maxY - b.minY + 60)),
      );
      const view = {
        k,
        x: width / 2 - k * ((b.minX + b.maxX) / 2),
        y: height / 2 - k * ((b.minY + b.maxY) / 2),
      };
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      draw(ctx, {
        graph,
        view,
        width,
        height,
        dpr,
        palette: readPalette(),
        focus: null,
        selected: null,
        hovered: null,
        matches: null,
        pulse: 1,
        anchors,
        visibleWidth: width,
      });
    };
    paint();
    const observer = new ResizeObserver(paint);
    observer.observe(wrap);
    const theme = new MutationObserver(paint);
    theme.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    document.fonts?.ready.then(paint);
    return () => {
      observer.disconnect();
      theme.disconnect();
    };
  }, [data]);

  return (
    <button
      type="button"
      className="preview"
      onClick={onOpen}
      aria-label="Open the graph in full view"
    >
      <div className="preview-canvas" ref={wrapRef}>
        <canvas ref={canvasRef} aria-hidden />
      </div>
      <span className="preview-cta">
        Open full view <Maximize2 aria-hidden />
      </span>
    </button>
  );
}

interface OverviewProps {
  ready: boolean;
  onNavigate: (tab: Tab) => void;
  /** Opens the graph in full view. */
  onOpenGraph: () => void;
  onOpenFile: (id: string) => void;
  onAddFiles: () => void;
}

/** The home screen: what Gather holds, what needs you, what came in lately. */
export default function Overview({
  ready,
  onNavigate,
  onOpenGraph,
  onOpenFile,
  onAddFiles,
}: OverviewProps) {
  const [snap, setSnap] = useState<Snapshot | null>(null);

  useEffect(() => {
    if (!ready) return;
    let cancelled = false;
    const load = async () => {
      const [files, review, contradictions, merges, graph] = await Promise.allSettled([
        listArtifacts(FILE_CAP + 1, 0),
        listReview(100),
        listContradictions("open"),
        listMergeSuggestions(),
        getGraphOverview(60, 40),
      ]);
      if (cancelled) return;
      const value = <T,>(r: PromiseSettledResult<T>, fallback: T) =>
        r.status === "fulfilled" ? r.value : fallback;
      setSnap({
        files: value(files, []),
        review: value(review, []),
        contradictions: value(contradictions, []),
        merges: value(merges, []),
        graph: value(graph, null),
      });
    };
    load();
    const timer = setInterval(load, 20_000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [ready]);

  const needs = snap ? snap.review.length + snap.contradictions.length + snap.merges.length : 0;
  const fileCount = snap ? Math.min(snap.files.length, FILE_CAP) : 0;
  const fileLabel = snap && snap.files.length > FILE_CAP ? `${FILE_CAP}+` : `${fileCount}`;
  const empty = snap !== null && snap.files.length === 0;

  const summary = !snap
    ? "Getting your library…"
    : empty
      ? "Your library is empty. Drop files anywhere in this window to begin."
      : `${plural(fileCount, "file")}, ${plural(snap.graph?.entity_total ?? 0, "entity", "entities")} linked. ${
          needs === 0 ? "Nothing needs you." : `${plural(needs, "thing")} could use a look.`
        }`;

  return (
    <>
      <Toolbar title="Overview" icon={House}>
        <Button variant="primary" size="sm" icon={Plus} onClick={onAddFiles} disabled={!ready}>
          Add files
        </Button>
      </Toolbar>
      <div className="page">
        <div className="page-inner overview">
          <section className="hero">
            <div className="hero-glow" aria-hidden />
            <div className="hero-text">
              <p className="hero-kicker">
                <ShieldCheck aria-hidden /> Private · on this computer
              </p>
              <h2 className="hero-title">{greeting()}</h2>
              <p className="hero-sub">{summary}</p>
            </div>
            <div className="hero-drop" aria-hidden>
              <Upload />
              <span>Drop files anywhere</span>
            </div>
          </section>

          {!snap ? (
            <Skeleton rows={3} variant="card" />
          ) : empty ? (
            <section className="onboard">
              {[
                {
                  icon: ScanText,
                  title: "Add what you have",
                  body: "PDFs, notes, chat exports, screenshots and photos. Everything is read locally.",
                },
                {
                  icon: Waypoints,
                  title: "Gather connects it",
                  body: "People, places, tools and decisions become a graph, each with its source.",
                },
                {
                  icon: GitCompareArrows,
                  title: "You settle the rest",
                  body: "Duplicates merge on their own; disagreements are flagged for a quick decision.",
                },
              ].map(({ icon: Icon, title, body }, i) => (
                <div className="onboard-step" key={title} style={{ animationDelay: `${i * 70}ms` }}>
                  <span className="onboard-icon" aria-hidden>
                    <Icon />
                  </span>
                  <span className="onboard-num num" aria-hidden>
                    0{i + 1}
                  </span>
                  <h3>{title}</h3>
                  <p>{body}</p>
                </div>
              ))}
              <div className="onboard-cta">
                <Button
                  variant="primary"
                  size="lg"
                  icon={Plus}
                  onClick={onAddFiles}
                  disabled={!ready}
                >
                  Choose files…
                </Button>
              </div>
            </section>
          ) : (
            <>
              <div className="stats">
                <StatTile
                  icon={FileText}
                  label="Files"
                  value={fileLabel}
                  tone="accent"
                  onClick={() => onNavigate("library")}
                />
                <StatTile
                  icon={Waypoints}
                  label="Entities"
                  value={(snap.graph?.entity_total ?? 0).toLocaleString()}
                  tone="info"
                  onClick={() => onNavigate("graph")}
                />
                <StatTile
                  icon={GitCompareArrows}
                  label="Open contradictions"
                  value={snap.contradictions.length}
                  tone={snap.contradictions.length > 0 ? "danger" : "neutral"}
                  onClick={() => onNavigate("contradictions")}
                />
                <StatTile
                  icon={Inbox}
                  label="In review"
                  value={snap.review.length}
                  tone={snap.review.length > 0 ? "warning" : "neutral"}
                  onClick={() => onNavigate("review")}
                />
              </div>

              <div className="overview-grid">
                <Panel
                  title="Your graph"
                  icon={Waypoints}
                  className="overview-graph"
                  actions={
                    snap.graph?.truncated && (
                      <span className="hint">{snap.graph.entities.length} most connected</span>
                    )
                  }
                >
                  {snap.graph && snap.graph.entities.length > 0 ? (
                    <GraphPreview data={snap.graph} onOpen={onOpenGraph} />
                  ) : (
                    <p className="panel-pad hint">
                      The graph fills in as Gather finds people, places and ideas in your files.
                    </p>
                  )}
                </Panel>

                <Panel title="Needs you" icon={Sparkles} className="overview-needs">
                  {needs === 0 ? (
                    <p className="panel-pad hint">
                      All clear. Gather decided everything on its own.
                    </p>
                  ) : (
                    <ul className="rows panel-rows">
                      {snap.contradictions.slice(0, 3).map((c) => (
                        <li key={c.id}>
                          <button
                            type="button"
                            className="row"
                            onClick={() => onNavigate("contradictions")}
                          >
                            <span className="row-lead lead-danger" aria-hidden>
                              <GitCompareArrows />
                            </span>
                            <span className="row-main">
                              <span className="row-title">{c.unit_a.statement}</span>
                              <span className="row-meta">vs “{c.unit_b.statement}”</span>
                            </span>
                          </button>
                        </li>
                      ))}
                      {snap.review.slice(0, 3).map((r) => (
                        <li key={r.id}>
                          <button
                            type="button"
                            className="row"
                            onClick={() => onNavigate("review")}
                          >
                            <span className="row-lead lead-warning" aria-hidden>
                              <Inbox />
                            </span>
                            <span className="row-main">
                              <span className="row-title">
                                {r.statement ??
                                  (r.a_name && r.b_name
                                    ? `${r.a_name} ≈ ${r.b_name}`
                                    : "Review item")}
                              </span>
                              <span className="row-meta">
                                {r.reason === "low-confidence"
                                  ? "Unsure fact"
                                  : "Possible duplicate"}
                              </span>
                            </span>
                          </button>
                        </li>
                      ))}
                      {snap.merges.slice(0, 2).map((m) => (
                        <li key={`${m.a.id}:${m.b.id}`}>
                          <button
                            type="button"
                            className="row"
                            onClick={() => onNavigate("entities")}
                          >
                            <span className="row-lead lead-info" aria-hidden>
                              <Combine />
                            </span>
                            <span className="row-main">
                              <span className="row-title">
                                {m.a.name} ≈ {m.b.name}
                              </span>
                              <span className="row-meta">Same {m.a.kind}?</span>
                            </span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                </Panel>

                <Panel
                  title="Recently added"
                  icon={FileText}
                  className="overview-recent"
                  actions={
                    <Button variant="ghost" size="sm" onClick={() => onNavigate("library")}>
                      Open Library
                    </Button>
                  }
                >
                  <ul className="recent">
                    {snap.files.slice(0, 6).map((f) => {
                      const Icon = kindIcon(f.kind);
                      return (
                        <li key={f.id}>
                          <button
                            type="button"
                            className="recent-item"
                            onClick={() => onOpenFile(f.id)}
                          >
                            <span className="recent-icon" aria-hidden>
                              <Icon />
                            </span>
                            <span className="recent-name">
                              {f.original_filename ?? `(${f.source_platform} import)`}
                            </span>
                            <span className="recent-meta">
                              {kindLabel(f.kind)}
                              <span className="dot-sep">
                                <When iso={f.ingested_at} />
                              </span>
                            </span>
                            {f.status === "processing" ? (
                              <Badge tone="warning">Reading</Badge>
                            ) : f.status === "failed" ? (
                              <Badge tone="danger">Unreadable</Badge>
                            ) : (
                              <Badge tone={f.unit_count > 0 ? "accent" : "neutral"}>
                                {plural(f.unit_count, "item")}
                              </Badge>
                            )}
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </Panel>
              </div>
            </>
          )}
        </div>
      </div>
    </>
  );
}
