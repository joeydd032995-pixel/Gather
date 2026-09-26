import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  forceX,
  forceY,
  type Simulation,
} from "d3-force";
import {
  FileText,
  Maximize2,
  Minimize2,
  Minus,
  Plus,
  Scan,
  Search,
  Waypoints,
  X,
} from "lucide-react";
import { getGraphOverview, type GraphOverview } from "./api";
import { EntityPanel, FilePanel } from "./graph/Panels";
import {
  ENTITY_KINDS,
  buildGraph,
  kindKey,
  type Graph as GraphModel,
  type Link,
  type Node,
} from "./graph/model";
import {
  bounds,
  draw,
  drawMinimap,
  hitTest,
  readPalette,
  type MinimapFrame,
  type Palette,
  type View,
} from "./graph/render";
import { kindLabel, plural } from "./kinds";
import { isTauri, setFullscreen } from "./native";
import { Callout, EmptyState, IconButton, Kbd, Spinner, Toolbar, errorText } from "./ui";

const SIZES = [50, 150, 400];
const MIN_K = 0.15;
const MAX_K = 5;
const MINIMAP_W = 176;
const MINIMAP_H = 116;
/** The detail drawer's width, which the camera leaves room for. */
const DRAWER_W = 360;

const reducedMotion = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
const clampK = (k: number) => Math.min(MAX_K, Math.max(MIN_K, k));
const ease = (t: number) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);

interface GraphProps {
  onOpenFile: (id: string) => void;
  /** Open straight into full view, as the Overview preview does. */
  initialFocus?: boolean;
}

function isTyping(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  );
}

export default function Graph({ onOpenFile, initialFocus = false }: GraphProps) {
  const [size, setSize] = useState(150);
  const [showFiles, setShowFiles] = useState(true);
  const [hiddenKinds, setHiddenKinds] = useState<ReadonlySet<string>>(new Set());
  const [data, setData] = useState<GraphOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [activeResult, setActiveResult] = useState(0);
  const [searchOpen, setSearchOpen] = useState(false);
  const [zoom, setZoom] = useState(100);
  const [touched, setTouched] = useState(false);
  const [version, setVersion] = useState(0);

  const stageRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const miniRef = useRef<HTMLCanvasElement>(null);
  const hoverCardRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const graphRef = useRef<GraphModel | null>(null);
  const simRef = useRef<Simulation<Node, Link> | null>(null);
  const viewRef = useRef<View>({ x: 0, y: 0, k: 1 });
  const sizeRef = useRef({ w: 800, h: 560, dpr: 1 });
  const paletteRef = useRef<Palette | null>(null);
  const miniFrame = useRef<MinimapFrame | null>(null);
  const frame = useRef(0);
  const tween = useRef(0);
  const pulseStart = useRef(0);
  const interacted = useRef(false);
  const selectedRef = useRef(selected);
  const hoveredRef = useRef(hovered);
  const matchesRef = useRef<Set<string> | null>(null);
  const anchorsRef = useRef<Set<string>>(new Set());
  selectedRef.current = selected;
  hoveredRef.current = hovered;

  // ------------------------------------------------------------ drawing
  /** The part of the stage not covered by the drawer. */
  const visibleArea = useCallback(() => {
    const { w, h } = sizeRef.current;
    if (selectedRef.current === null) return { w, h };
    // A side drawer on wide stages, a bottom sheet (52% tall) on narrow ones.
    return w > 720 ? { w: w - DRAWER_W - 16, h } : { w, h: h * 0.48 };
  }, []);

  const render = useCallback(() => {
    frame.current = 0;
    const canvas = canvasRef.current;
    const graph = graphRef.current;
    if (!canvas || !graph) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    paletteRef.current ??= readPalette();
    const { w, h, dpr } = sizeRef.current;
    const pulse = Math.min(1, (performance.now() - pulseStart.current) / 650);
    const scene = {
      graph,
      view: viewRef.current,
      width: w,
      height: h,
      dpr,
      palette: paletteRef.current,
      focus: hoveredRef.current ?? selectedRef.current,
      selected: selectedRef.current,
      hovered: hoveredRef.current,
      matches: matchesRef.current,
      pulse,
      anchors: anchorsRef.current,
      visibleWidth: visibleArea().w,
    };
    draw(ctx, scene);
    const mini = miniRef.current?.getContext("2d");
    if (mini) miniFrame.current = drawMinimap(mini, scene, MINIMAP_W, MINIMAP_H);

    // Keep the hover card pinned to its node as the layout settles.
    const card = hoverCardRef.current;
    const hoveredNode = hoveredRef.current ? graph.byKey.get(hoveredRef.current) : undefined;
    if (card && hoveredNode?.x !== undefined) {
      const v = viewRef.current;
      const x = hoveredNode.x * v.k + v.x;
      const y = (hoveredNode.y! - hoveredNode.r) * v.k + v.y - 10;
      card.style.transform = `translate(${Math.round(x)}px, ${Math.round(y)}px) translate(-50%, -100%)`;
    }
    const pct = Math.round(viewRef.current.k * 100);
    setZoom((z) => (z === pct ? z : pct));
    if (pulse < 1) frame.current = requestAnimationFrame(render);
  }, [visibleArea]);

  const requestDraw = useCallback(() => {
    if (!frame.current) frame.current = requestAnimationFrame(render);
  }, [render]);

  const setView = useCallback(
    (v: View) => {
      viewRef.current = v;
      requestDraw();
    },
    [requestDraw],
  );

  /** Glide the camera to `to`; instant when motion is reduced. */
  const animateTo = useCallback(
    (to: View, duration = 520) => {
      cancelAnimationFrame(tween.current);
      if (reducedMotion()) return setView(to);
      const from = { ...viewRef.current };
      const start = performance.now();
      const step = (now: number) => {
        const t = Math.min(1, (now - start) / duration);
        const e = ease(t);
        setView({
          x: from.x + (to.x - from.x) * e,
          y: from.y + (to.y - from.y) * e,
          k: from.k + (to.k - from.k) * e,
        });
        if (t < 1) tween.current = requestAnimationFrame(step);
      };
      tween.current = requestAnimationFrame(step);
    },
    [setView],
  );

  const fit = useCallback(
    (animated = true) => {
      const graph = graphRef.current;
      if (!graph) return;
      // Frame what's connected; a stray unlinked file shouldn't shrink everything.
      const linked = graph.nodes.filter((n) => n.degree > 0);
      const b = bounds(linked.length > 0 ? linked : graph.nodes);
      if (!b) return;
      const { w, h } = visibleArea();
      const k = clampK(
        Math.min(2, 0.86 * Math.min(w / (b.maxX - b.minX + 80), h / (b.maxY - b.minY + 80))),
      );
      const to = {
        k,
        x: w / 2 - k * ((b.minX + b.maxX) / 2),
        y: h / 2 - k * ((b.minY + b.maxY) / 2),
      };
      if (animated) animateTo(to);
      else setView(to);
    },
    [animateTo, setView, visibleArea],
  );

  const centreOn = useCallback(
    (node: Node, k = Math.max(viewRef.current.k, 1.25)) => {
      if (node.x === undefined || node.y === undefined) return;
      const { w, h } = visibleArea();
      animateTo({ k, x: w / 2 - k * node.x, y: h / 2 - k * node.y });
    },
    [animateTo, visibleArea],
  );

  const select = useCallback(
    (key: string | null, move = true) => {
      selectedRef.current = key;
      setSelected(key);
      pulseStart.current = performance.now();
      const node = key ? graphRef.current?.byKey.get(key) : undefined;
      if (node && move) {
        interacted.current = true;
        centreOn(node);
      }
      requestDraw();
    },
    [centreOn, requestDraw],
  );

  // ------------------------------------------------------------ full view
  // Full view gives the graph the whole screen: the app's sidebar and toolbar
  // step aside and the window goes full screen; both come back on exit.
  const [focus, setFocus] = useState(initialFocus);
  const [exitHint, setExitHint] = useState(false);

  useEffect(() => {
    if (!focus) return;
    const changed = setFullscreen(true).catch(() => false);
    setExitHint(true);
    const hint = setTimeout(() => setExitHint(false), 2600);
    // A browser leaves full screen on Esc without a keydown; follow it out.
    const onChange = () => {
      if (!isTauri && !document.fullscreenElement) changed.then((c) => c && setFocus(false));
    };
    document.addEventListener("fullscreenchange", onChange);
    return () => {
      clearTimeout(hint);
      setExitHint(false);
      document.removeEventListener("fullscreenchange", onChange);
      changed.then((c) => c && setFullscreen(false).catch(() => {}));
    };
  }, [focus]);

  // Reframe once the stage has settled at its new size.
  const reframe = useRef(() => {});
  reframe.current = () => {
    const node = selectedRef.current ? graphRef.current?.byKey.get(selectedRef.current) : undefined;
    if (node) centreOn(node);
    else fit();
  };
  const focusMounted = useRef(false);
  useEffect(() => {
    if (!focusMounted.current) {
      focusMounted.current = true;
      return;
    }
    const timer = setTimeout(() => reframe.current(), 320);
    return () => clearTimeout(timer);
  }, [focus]);

  // F toggles full view; Esc first clears the selection, then leaves full view.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (isTyping(e.target) || e.metaKey || e.ctrlKey || e.altKey) return;
      if (document.querySelector('[role="dialog"]')) return;
      if (e.key === "f" || e.key === "F") {
        e.preventDefault();
        setFocus((f) => !f);
      } else if (e.key === "Escape") {
        if (selectedRef.current) select(null, false);
        else if (focus) setFocus(false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [focus, select]);

  // --------------------------------------------------------------- data
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

  // Build and lay out. Positions carry over, so filters rearrange gently.
  useEffect(() => {
    if (!data) return;
    const previous = graphRef.current?.byKey;
    const graph = buildGraph(data, { showFiles, hiddenKinds, previous });
    graphRef.current = graph;
    anchorsRef.current = new Set(
      graph.nodes
        .filter((n) => n.type === "entity")
        .sort((a, b) => b.weight - a.weight)
        .slice(0, 16)
        .map((n) => n.key),
    );
    if (selectedRef.current && !graph.byKey.has(selectedRef.current)) select(null, false);
    setVersion((v) => v + 1);

    const warm = previous !== undefined && previous.size > 0;
    const simulation = forceSimulation<Node, Link>(graph.nodes)
      .force(
        "link",
        forceLink<Node, Link>(graph.links)
          .id((n) => n.key)
          .distance((l) => (l.type === "mention" ? 80 : 64))
          .strength((l) => (l.type === "mention" ? 0.18 : 0.55)),
      )
      .force("charge", forceManyBody<Node>().strength(-220).distanceMax(520))
      .force("center", forceCenter(0, 0))
      // A light pull inwards keeps loose pieces from drifting off-screen.
      .force(
        "x",
        forceX<Node>(0).strength((n) => (n.degree === 0 ? 0.15 : 0.035)),
      )
      .force(
        "y",
        forceY<Node>(0).strength((n) => (n.degree === 0 ? 0.15 : 0.035)),
      )
      .force(
        "collide",
        forceCollide<Node>((n) => n.r + (n.type === "entity" ? 14 : 6)).strength(0.9),
      )
      .alpha(warm ? 0.35 : 1)
      .alphaDecay(0.04)
      .on("tick", requestDraw)
      .on("end", () => {
        if (!interacted.current) fit();
      });
    simRef.current = simulation;
    // Fit early too, so the first frames aren't a speck in the middle.
    const early = setTimeout(() => !interacted.current && fit(), 450);
    return () => {
      clearTimeout(early);
      simulation.stop();
    };
  }, [data, showFiles, hiddenKinds, fit, requestDraw, select]);

  // ------------------------------------------------------ size and theme
  useEffect(() => {
    const stage = stageRef.current;
    const canvas = canvasRef.current;
    if (!stage || !canvas) return;
    const observer = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      const dpr = Math.min(2, window.devicePixelRatio || 1);
      const first = sizeRef.current.w === 800 && sizeRef.current.h === 560;
      const old = sizeRef.current;
      sizeRef.current = { w: width, h: height, dpr };
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
      const mini = miniRef.current;
      if (mini) {
        mini.width = MINIMAP_W * dpr;
        mini.height = MINIMAP_H * dpr;
      }
      // Keep the same point in the middle as the stage resizes.
      const v = viewRef.current;
      viewRef.current = first
        ? { k: 1, x: width / 2, y: height / 2 }
        : { ...v, x: v.x + (width - old.w) / 2, y: v.y + (height - old.h) / 2 };
      requestDraw();
    });
    observer.observe(stage);
    return () => observer.disconnect();
  }, [data, requestDraw]);

  useEffect(() => {
    const refresh = () => {
      paletteRef.current = readPalette();
      requestDraw();
    };
    const media = matchMedia("(prefers-color-scheme: dark)");
    media.addEventListener("change", refresh);
    const observer = new MutationObserver(refresh);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    document.fonts?.ready.then(refresh);
    return () => {
      media.removeEventListener("change", refresh);
      observer.disconnect();
    };
  }, [requestDraw]);

  useEffect(
    () => () => {
      cancelAnimationFrame(frame.current);
      cancelAnimationFrame(tween.current);
      // A remount (StrictMode does one) must be able to schedule again.
      frame.current = 0;
    },
    [],
  );

  // -------------------------------------------------------------- search
  const needle = query.trim().toLowerCase();
  const results = useMemo(() => {
    const graph = graphRef.current;
    if (!needle || !graph) return [];
    return graph.nodes
      .filter((n) => n.name.toLowerCase().includes(needle))
      .sort(
        (a, b) =>
          Number(b.name.toLowerCase().startsWith(needle)) -
            Number(a.name.toLowerCase().startsWith(needle)) || b.weight - a.weight,
      );
    // `version` changes whenever the graph is rebuilt.
  }, [needle, version]);

  useEffect(() => {
    matchesRef.current = needle ? new Set(results.map((n) => n.key)) : null;
    setActiveResult(0);
    requestDraw();
  }, [needle, results, requestDraw]);

  useEffect(requestDraw, [selected, hovered, requestDraw]);

  const pick = (node: Node) => {
    select(node.key);
    setSearchOpen(false);
  };

  // ------------------------------------------------------------ pointers
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef<{
    node: Node | null;
    startX: number;
    startY: number;
    view: View;
    moved: boolean;
    pinch?: { dist: number; mx: number; my: number; view: View };
  } | null>(null);

  const local = (e: { clientX: number; clientY: number }) => {
    const rect = canvasRef.current!.getBoundingClientRect();
    return { x: e.clientX - rect.left, y: e.clientY - rect.top };
  };

  const noteInteraction = () => {
    interacted.current = true;
    cancelAnimationFrame(tween.current);
    if (!touched) setTouched(true);
  };

  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const graph = graphRef.current;
    if (!graph) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    const p = local(e);
    pointers.current.set(e.pointerId, p);
    if (pointers.current.size === 2) {
      const [a, b] = [...pointers.current.values()];
      gesture.current = {
        node: null,
        startX: p.x,
        startY: p.y,
        view: { ...viewRef.current },
        moved: true,
        pinch: {
          dist: Math.hypot(a.x - b.x, a.y - b.y),
          mx: (a.x + b.x) / 2,
          my: (a.y + b.y) / 2,
          view: { ...viewRef.current },
        },
      };
      return;
    }
    gesture.current = {
      node: hitTest(graph, viewRef.current, p.x, p.y),
      startX: p.x,
      startY: p.y,
      view: { ...viewRef.current },
      moved: false,
    };
  };

  const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const graph = graphRef.current;
    if (!graph) return;
    const p = local(e);
    if (pointers.current.has(e.pointerId)) pointers.current.set(e.pointerId, p);
    const g = gesture.current;

    if (!g) {
      // Once you're pointing at something, the camera stops moving on its own.
      interacted.current = true;
      const hit = hitTest(graph, viewRef.current, p.x, p.y);
      const key = hit?.key ?? null;
      if (key !== hoveredRef.current) {
        hoveredRef.current = key;
        setHovered(key);
      }
      e.currentTarget.style.cursor = hit ? "pointer" : "grab";
      return;
    }

    if (g.pinch && pointers.current.size >= 2) {
      const [a, b] = [...pointers.current.values()];
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      const mx = (a.x + b.x) / 2;
      const my = (a.y + b.y) / 2;
      const v = g.pinch.view;
      const k = clampK(v.k * (dist / g.pinch.dist));
      setView({
        k,
        x: mx - ((g.pinch.mx - v.x) / v.k) * k,
        y: my - ((g.pinch.my - v.y) / v.k) * k,
      });
      return;
    }

    if (!g.moved && Math.hypot(p.x - g.startX, p.y - g.startY) > 4) {
      g.moved = true;
      noteInteraction();
      if (hoveredRef.current) {
        hoveredRef.current = null;
        setHovered(null);
      }
    }
    if (!g.moved) return;
    if (g.node) {
      const v = viewRef.current;
      g.node.fx = (p.x - v.x) / v.k;
      g.node.fy = (p.y - v.y) / v.k;
      simRef.current?.alphaTarget(0.25).restart();
    } else {
      e.currentTarget.style.cursor = "grabbing";
      setView({ ...g.view, x: g.view.x + p.x - g.startX, y: g.view.y + p.y - g.startY });
    }
  };

  const onPointerUp = (e: React.PointerEvent<HTMLCanvasElement>) => {
    pointers.current.delete(e.pointerId);
    const g = gesture.current;
    if (pointers.current.size > 0) return;
    gesture.current = null;
    e.currentTarget.style.cursor = "grab";
    if (!g) return;
    if (g.node && g.moved) {
      g.node.fx = null;
      g.node.fy = null;
      simRef.current?.alphaTarget(0);
    }
    if (!g.moved) {
      noteInteraction();
      select(g.node ? g.node.key : null, false);
      // Keep what you clicked clear of the drawer that just opened.
      const node = g.node;
      if (node?.x !== undefined) {
        const v = viewRef.current;
        const sx = node.x * v.k + v.x;
        const sy = node.y! * v.k + v.y;
        const { w, h } = visibleArea();
        if (sx > w - 60 || sy > h - 60) centreOn(node, v.k);
      }
    }
  };

  const onPointerLeave = () => {
    if (hoveredRef.current && !gesture.current) {
      hoveredRef.current = null;
      setHovered(null);
    }
  };

  const onDoubleClick = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const graph = graphRef.current;
    if (!graph) return;
    const p = local(e);
    const hit = hitTest(graph, viewRef.current, p.x, p.y);
    noteInteraction();
    if (hit) {
      select(hit.key, false);
      centreOn(hit, Math.min(MAX_K, viewRef.current.k * 1.8));
      return;
    }
    const v = viewRef.current;
    const k = clampK(v.k * 1.8);
    animateTo({ k, x: p.x - ((p.x - v.x) / v.k) * k, y: p.y - ((p.y - v.y) / v.k) * k }, 360);
  };

  // Wheel zoom around the cursor (non-passive, so the page doesn't scroll).
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      noteInteraction();
      const p = local(e);
      const v = viewRef.current;
      // Trackpad pinches arrive as ctrl+wheel with small deltas.
      const factor = Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0016));
      const k = clampK(v.k * factor);
      setView({ k, x: p.x - ((p.x - v.x) / v.k) * k, y: p.y - ((p.y - v.y) / v.k) * k });
    };
    canvas.addEventListener("wheel", onWheel, { passive: false });
    return () => canvas.removeEventListener("wheel", onWheel);
    // noteInteraction only reads refs and a setter.
  }, [data, setView]);

  const zoomBy = (factor: number) => {
    noteInteraction();
    const v = viewRef.current;
    const { w, h } = visibleArea();
    const k = clampK(v.k * factor);
    animateTo(
      { k, x: w / 2 - ((w / 2 - v.x) / v.k) * k, y: h / 2 - ((h / 2 - v.y) / v.k) * k },
      260,
    );
  };

  const onCanvasKey = (e: React.KeyboardEvent) => {
    const v = viewRef.current;
    const pan = (dx: number, dy: number) => {
      noteInteraction();
      animateTo({ ...v, x: v.x + dx, y: v.y + dy }, 180);
    };
    switch (e.key) {
      case "ArrowLeft":
        return pan(80, 0);
      case "ArrowRight":
        return pan(-80, 0);
      case "ArrowUp":
        return pan(0, 80);
      case "ArrowDown":
        return pan(0, -80);
      case "+":
      case "=":
        return zoomBy(1.3);
      case "-":
        return zoomBy(1 / 1.3);
      case "0":
        return fit();
      case "/":
        e.preventDefault();
        return searchRef.current?.focus();
      default:
        return;
    }
  };

  const onMinimap = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const f = miniFrame.current;
    if (!f || (e.type === "pointermove" && e.buttons !== 1)) return;
    if (e.type === "pointerdown") e.currentTarget.setPointerCapture(e.pointerId);
    const rect = e.currentTarget.getBoundingClientRect();
    const gx = (e.clientX - rect.left - f.ox) / f.scale;
    const gy = (e.clientY - rect.top - f.oy) / f.scale;
    const v = viewRef.current;
    const { w, h } = visibleArea();
    const to = { k: v.k, x: w / 2 - v.k * gx, y: h / 2 - v.k * gy };
    noteInteraction();
    if (e.type === "pointerdown") animateTo(to, 300);
    else setView(to);
  };

  // --------------------------------------------------------------- view
  const header = (
    <Toolbar
      title="Graph"
      icon={Waypoints}
      count={
        data && data.entities.length > 0
          ? data.truncated
            ? `${data.entities.length} of ${data.entity_total.toLocaleString()}`
            : data.entities.length
          : undefined
      }
    >
      <IconButton icon={Maximize2} label="Full view (F)" size="sm" onClick={() => setFocus(true)} />
      <span className="toolbar-sep" aria-hidden />
      <label className="toolbar-field">
        <span>Show</span>
        <select className="select" value={size} onChange={(e) => setSize(Number(e.target.value))}>
          {SIZES.map((s) => (
            <option key={s} value={s}>
              Top {s}
            </option>
          ))}
        </select>
      </label>
    </Toolbar>
  );

  if (error) {
    return (
      <>
        {header}
        <div className="view-callout">
          <Callout title="Couldn't load the graph">{error}</Callout>
        </div>
      </>
    );
  }
  if (!data) {
    return (
      <>
        {header}
        <div className="explorer explorer-loading">
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

  const graph = graphRef.current;
  const kindCounts = new Map<string, number>();
  for (const e of data.entities)
    kindCounts.set(kindKey(e.kind), (kindCounts.get(kindKey(e.kind)) ?? 0) + 1);
  const selectedNode = selected ? graph?.byKey.get(selected) : undefined;
  const hoveredNode = hovered && hovered !== selected ? graph?.byKey.get(hovered) : undefined;
  const shownResults = results.slice(0, 7);

  const toggleKind = (kind: string) => {
    noteInteraction();
    setHiddenKinds((prev) => {
      const next = new Set(prev);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
  };

  return (
    <div className={focus ? "graph-view is-focus" : "graph-view"}>
      {!focus && header}
      <div className={selectedNode ? "explorer has-drawer" : "explorer"} ref={stageRef}>
        <canvas
          ref={canvasRef}
          className="explorer-canvas"
          tabIndex={0}
          role="application"
          aria-roledescription="graph"
          aria-label={`Graph of ${plural(graph?.nodes.length ?? 0, "item")} and ${plural(
            graph?.links.length ?? 0,
            "connection",
          )}. Arrow keys pan, plus and minus zoom, 0 fits, slash searches, F toggles full view.`}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerUp}
          onPointerLeave={onPointerLeave}
          onDoubleClick={onDoubleClick}
          onKeyDown={onCanvasKey}
        />

        {/* search and filters */}
        <div className="explorer-bar glass">
          <div className="explorer-search">
            <Search aria-hidden />
            <input
              ref={searchRef}
              type="search"
              role="combobox"
              aria-expanded={searchOpen && shownResults.length > 0}
              aria-controls="graph-results"
              aria-activedescendant={
                searchOpen && shownResults[activeResult]
                  ? `graph-result-${activeResult}`
                  : undefined
              }
              aria-label="Find in graph"
              placeholder="Find a person, place, file…"
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                setSearchOpen(true);
              }}
              onFocus={() => setSearchOpen(true)}
              onBlur={() => setTimeout(() => setSearchOpen(false), 120)}
              onKeyDown={(e) => {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setActiveResult((i) => Math.min(i + 1, shownResults.length - 1));
                } else if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setActiveResult((i) => Math.max(i - 1, 0));
                } else if (e.key === "Enter" && shownResults[activeResult]) {
                  pick(shownResults[activeResult]);
                } else if (e.key === "Escape") {
                  setQuery("");
                  canvasRef.current?.focus();
                }
              }}
            />
            {query ? <span className="explorer-count num">{results.length}</span> : <Kbd>/</Kbd>}
          </div>

          {searchOpen && needle && (
            <ul className="explorer-results glass" id="graph-results" role="listbox">
              {shownResults.length === 0 && <li className="explorer-noresult">No matches</li>}
              {shownResults.map((n, i) => (
                <li
                  key={n.key}
                  id={`graph-result-${i}`}
                  role="option"
                  aria-selected={i === activeResult}
                  className="explorer-result"
                  onMouseDown={(e) => e.preventDefault()}
                  onMouseMove={() => setActiveResult(i)}
                  onClick={() => pick(n)}
                >
                  {n.type === "file" ? (
                    <FileText className="explorer-result-icon" aria-hidden />
                  ) : (
                    <span className="relation-dot" data-kind={kindKey(n.kind)} aria-hidden />
                  )}
                  <span className="explorer-result-name">{n.name}</span>
                  <span className="explorer-result-meta">
                    {n.type === "file" ? kindLabel(n.kind) : n.kind}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>

        {/* zoom */}
        <div className="explorer-zoom glass" role="group" aria-label="Zoom">
          <IconButton icon={Plus} label="Zoom in (+)" size="sm" onClick={() => zoomBy(1.35)} />
          <span className="explorer-zoom-value num" aria-live="off">
            {zoom}%
          </span>
          <IconButton
            icon={Minus}
            label="Zoom out (−)"
            size="sm"
            onClick={() => zoomBy(1 / 1.35)}
          />
          <span className="explorer-divider-h" aria-hidden />
          <IconButton icon={Scan} label="Fit everything (0)" size="sm" onClick={() => fit()} />
          <span className="explorer-divider-h" aria-hidden />
          <IconButton
            icon={focus ? Minimize2 : Maximize2}
            label={focus ? "Exit full view (Esc)" : "Full view (F)"}
            size="sm"
            onClick={() => setFocus((f) => !f)}
          />
        </div>

        {focus && exitHint && (
          <div className="explorer-toast glass" role="status">
            Press <Kbd>Esc</Kbd> to exit full view
          </div>
        )}

        {/* legend: also a filter */}
        <div className="explorer-legend glass" role="group" aria-label="Show or hide kinds">
          {ENTITY_KINDS.filter((k) => kindCounts.has(k)).map((kind) => {
            const on = !hiddenKinds.has(kind);
            return (
              <button
                key={kind}
                type="button"
                className={on ? "legend-chip" : "legend-chip off"}
                aria-pressed={on}
                data-kind={kind}
                onClick={() => toggleKind(kind)}
                title={on ? `Hide ${kind}` : `Show ${kind}`}
              >
                <span className="legend-dot" aria-hidden />
                {kind}
                <span className="legend-count num">{kindCounts.get(kind)}</span>
              </button>
            );
          })}
          <button
            type="button"
            className={showFiles ? "legend-chip" : "legend-chip off"}
            aria-pressed={showFiles}
            onClick={() => {
              noteInteraction();
              setShowFiles((s) => !s);
            }}
            title={showFiles ? "Hide files" : "Show files"}
          >
            <span className="legend-file" aria-hidden />
            files
            <span className="legend-count num">{data.files.length}</span>
          </button>
        </div>

        {/* minimap */}
        <div className="explorer-minimap glass" aria-hidden>
          <canvas
            ref={miniRef}
            style={{ width: MINIMAP_W, height: MINIMAP_H }}
            onPointerDown={onMinimap}
            onPointerMove={onMinimap}
          />
        </div>

        {!touched && !selectedNode && (
          <div className="explorer-hint glass" aria-hidden>
            <span>
              <b>Click</b> to explore
            </span>
            <span>
              <b>Drag</b> to pan
            </span>
            <span>
              <b>Scroll</b> to zoom
            </span>
            <span>
              <b>Double-click</b> to dive in
            </span>
          </div>
        )}

        {/* hover card, positioned by the render loop */}
        <div
          ref={hoverCardRef}
          className={hoveredNode ? "explorer-card glass visible" : "explorer-card glass"}
          aria-hidden
        >
          {hoveredNode && (
            <>
              <span className="explorer-card-name">
                {hoveredNode.type === "file" ? (
                  <FileText className="explorer-result-icon" />
                ) : (
                  <span className="relation-dot" data-kind={kindKey(hoveredNode.kind)} />
                )}
                {hoveredNode.name}
              </span>
              <span className="explorer-card-meta">
                {hoveredNode.type === "file" ? kindLabel(hoveredNode.kind) : hoveredNode.kind}
                <span className="dot-sep">{plural(hoveredNode.degree, "link")}</span>
              </span>
            </>
          )}
        </div>

        {selectedNode && graph && (
          <aside className="explorer-drawer glass" aria-label={`Details for ${selectedNode.name}`}>
            <IconButton
              icon={X}
              label="Close details (Esc)"
              size="sm"
              className="drawer-close"
              onClick={() => {
                select(null, false);
                canvasRef.current?.focus();
              }}
            />
            <div className="drawer-body" key={selectedNode.key}>
              {selectedNode.type === "entity" ? (
                <EntityPanel
                  node={selectedNode}
                  graph={graph}
                  onSelect={(key) => select(key)}
                  onOpenFile={onOpenFile}
                />
              ) : (
                <FilePanel
                  node={selectedNode}
                  graph={graph}
                  onSelect={(key) => select(key)}
                  onOpenFile={onOpenFile}
                />
              )}
            </div>
          </aside>
        )}
      </div>
    </div>
  );
}
