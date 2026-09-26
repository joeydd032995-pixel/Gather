import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowUpRight,
  Command as CommandIcon,
  LoaderCircle,
  Monitor,
  Moon,
  Plus,
  Search,
  Sun,
  TriangleAlert,
} from "lucide-react";
import { checkHealth, setApiToken, type HealthState } from "./api";
import Clusters from "./Clusters";
import Contradictions from "./Contradictions";
import Entities from "./Entities";
import Graph from "./Graph";
import { useAttention } from "./hooks/useAttention";
import { useRuntime } from "./hooks/useRuntime";
import { useTheme, type ThemeChoice } from "./hooks/useTheme";
import Library from "./Library";
import Logo from "./Logo";
import { checkForUpdate, getApiToken, getUpdateSettings, isTauri } from "./native";
import { NAV, NAV_ITEMS, type Tab } from "./nav";
import Overview from "./Overview";
import Palette, { type Command } from "./Palette";
import Photos from "./Photos";
import ReviewTray from "./ReviewTray";
import Settings from "./Settings";
import Tuning from "./Tuning";
import { Button, Callout, SectionContext } from "./ui";
import { DropOverlay, FallbackInput, UploadTray, useUploads } from "./uploads";

const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
const MOD = isMac ? "⌘" : "Ctrl+";

const THEME_ICONS: Record<ThemeChoice, typeof Sun> = { system: Monitor, light: Sun, dark: Moon };
const THEME_NEXT: Record<ThemeChoice, ThemeChoice> = {
  system: "light",
  light: "dark",
  dark: "system",
};

/** Start-up and failure screens, before the main window is usable. */
function Splash({ step, error, logDir }: { step?: string; error?: string; logDir?: string }) {
  return (
    <div className="splash">
      <div className="splash-glow" aria-hidden />
      <div className="splash-card">
        <Logo size={56} />
        <h1 className="splash-title">Gather</h1>
        {error ? (
          <>
            <Callout title="Gather couldn't start">{error}</Callout>
            {logDir && (
              <p className="hint">
                Logs are in <code>{logDir}</code>
              </p>
            )}
          </>
        ) : (
          <p className="splash-step" role="status" aria-live="polite">
            <LoaderCircle className="spin" aria-hidden />
            {step}…
          </p>
        )}
      </div>
    </div>
  );
}

function HealthPill({ health }: { health: HealthState }) {
  const state = health.ready ? "ok" : health.reachable ? "warn" : "down";
  const label = health.ready ? "Local · ready" : health.reachable ? "Database starting" : "Offline";
  const detail = health.ready
    ? "The local daemon and database are running. Everything stays on this computer."
    : health.reachable
      ? "The daemon is up but its database isn't ready yet."
      : "Gather's local daemon isn't reachable. Retrying every 5 seconds.";
  return (
    <div className={`health health-${state}`} title={detail} role="status" aria-live="polite">
      <span className="health-dot" aria-hidden />
      <span className="health-label">{label}</span>
    </div>
  );
}

export default function App() {
  const runtime = useRuntime();
  const settled = runtime.state === "ready" || runtime.state === "unmanaged";
  const [tab, setTab] = useState<Tab>("home");
  /** The file open in the Library; the graph and uploads set it too. */
  const [libraryFile, setLibraryFile] = useState<string | null>(null);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [health, setHealth] = useState<HealthState>({ reachable: false, ready: false });
  const [checkedHealth, setCheckedHealth] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [theme, setTheme] = useTheme();
  const mainRef = useRef<HTMLElement>(null);
  const navigated = useRef(false);
  const counts = useAttention(health.ready, tab);
  const uploads = useUploads();

  const [graphFocus, setGraphFocus] = useState(false);
  const go = useCallback((next: Tab) => {
    navigated.current = true;
    setGraphFocus(false);
    setTab(next);
  }, []);
  const openGraphFullView = useCallback(() => {
    navigated.current = true;
    setGraphFocus(true);
    setTab("graph");
  }, []);

  // In the packaged app, pick up the daemon's bearer token from the OS
  // keychain (written by the daemon in GATHER_AUTH_MODE=keychain). Waits for
  // start-up: on first run the daemon creates the token as it starts.
  useEffect(() => {
    if (!isTauri || !settled) return;
    getApiToken()
      .then((token) => {
        if (token) setApiToken(token);
      })
      .catch(() => {
        /* keychain empty or unavailable: dev daemons run open on loopback */
      });
  }, [settled]);

  // The opt-in update check: only when the user turned it on in Settings.
  useEffect(() => {
    if (!isTauri || !settled) return;
    getUpdateSettings()
      .then((s) => (s.check_on_start ? checkForUpdate() : null))
      .then((result) => {
        if (result?.available) setUpdateVersion(result.version);
      })
      .catch(() => {
        /* offline or unreachable: say nothing, try again next start */
      });
  }, [settled]);

  useEffect(() => {
    let cancelled = false;
    const poll = async () => {
      const h = await checkHealth();
      if (cancelled) return;
      setHealth(h);
      setCheckedHealth(true);
    };
    poll();
    const timer = setInterval(poll, 5000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  // A view change moves focus to its title, so screen readers announce it
  // and keyboard users start at the top of the new view.
  useEffect(() => {
    if (!navigated.current) return;
    mainRef.current
      ?.querySelector<HTMLElement>("[data-page-title]")
      ?.focus({ preventScroll: true });
  }, [tab]);

  const { pick } = uploads;
  const addFiles = useCallback(() => {
    if (health.ready) pick();
  }, [health.ready, pick]);

  // Global shortcuts: ⌘K palette, ⌘O add files.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey)) return;
      const key = e.key.toLowerCase();
      if (key === "k") {
        e.preventDefault();
        setPaletteOpen((open) => !open);
      } else if (key === "o") {
        e.preventDefault();
        addFiles();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [addFiles]);

  // Lets the visual checks in development drive navigation.
  useEffect(() => {
    if (!import.meta.env.DEV) return;
    (window as unknown as { __gatherNav?: (t: Tab) => void }).__gatherNav = go;
  }, [go]);

  const openFile = useCallback(
    (id: string) => {
      setLibraryFile(id);
      go("library");
    },
    [go],
  );

  const commands = useMemo<Command[]>(
    () => [
      ...NAV_ITEMS.map((item) => ({
        id: `go-${item.id}`,
        group: "Go to",
        label: item.label,
        hint: item.hint,
        icon: item.icon,
        run: () => go(item.id),
      })),
      {
        id: "add-files",
        group: "Actions",
        label: "Add files…",
        hint: `${MOD}O`,
        icon: Plus,
        run: addFiles,
      },
      ...(["light", "dark", "system"] as ThemeChoice[]).map((choice) => ({
        id: `theme-${choice}`,
        group: "Appearance",
        label: choice === "system" ? "Match system appearance" : `Use ${choice} appearance`,
        hint: theme === choice ? "current" : undefined,
        icon: THEME_ICONS[choice],
        run: () => setTheme(choice),
      })),
    ],
    [addFiles, go, setTheme, theme],
  );

  if (runtime.state === "starting") return <Splash step={runtime.step} />;
  if (runtime.state === "failed") {
    return <Splash error={runtime.message} logDir={runtime.log_dir} />;
  }

  const ThemeIcon = THEME_ICONS[theme];
  const section = NAV.find((g) => g.items.some((i) => i.id === tab))?.label ?? null;

  return (
    <div className="shell">
      <a className="skip-link" href="#content">
        Skip to content
      </a>

      <aside className="sidebar">
        <div className="brand">
          <Logo />
          <span className="brand-name">Gather</span>
          <button
            type="button"
            className="theme-toggle"
            onClick={() => setTheme(THEME_NEXT[theme])}
            aria-label={`Appearance: ${theme}. Switch to ${THEME_NEXT[theme]}.`}
            data-tip={`Appearance: ${theme}`}
            data-tip-side="bottom"
          >
            <ThemeIcon aria-hidden />
          </button>
        </div>

        <div className="sidebar-actions">
          <Button
            variant="primary"
            icon={Plus}
            className="add-button"
            onClick={addFiles}
            disabled={!health.ready}
            aria-label={`Add files (${MOD}O)`}
          >
            <span className="add-label">Add files</span>
          </Button>
          <button
            type="button"
            className="jump"
            onClick={() => setPaletteOpen(true)}
            aria-label={`Search or jump to… (${MOD}K)`}
            data-tip={`Jump to… ${MOD}K`}
            data-tip-side="right"
          >
            <Search aria-hidden />
            <span className="jump-label">Jump to…</span>
            <span className="jump-kbd" aria-hidden>
              {isMac ? <CommandIcon size={10} /> : "Ctrl"}
              <span>K</span>
            </span>
          </button>
        </div>

        <nav className="nav" aria-label="Main">
          {NAV.map((group) => (
            <div className="nav-group" key={group.label ?? "top"}>
              {group.label && <div className="nav-group-label">{group.label}</div>}
              <ul>
                {group.items.map((item) => {
                  const Icon = item.icon;
                  const count = counts[item.id];
                  const active = tab === item.id;
                  return (
                    <li key={item.id}>
                      <button
                        type="button"
                        className={active ? "nav-item active" : "nav-item"}
                        aria-current={active ? "page" : undefined}
                        onClick={() => go(item.id)}
                        data-tip={item.label}
                        data-tip-side="right"
                      >
                        <Icon className="nav-icon" aria-hidden />
                        <span className="nav-label">{item.label}</span>
                        {count !== undefined && count > 0 && (
                          <span className="nav-count num" aria-label={`${count} waiting`}>
                            {count > 99 ? "99+" : count}
                          </span>
                        )}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </nav>

        <div className="sidebar-foot">
          {updateVersion && (
            <button type="button" className="update-chip" onClick={() => go("settings")}>
              <ArrowUpRight aria-hidden />
              <span>Gather {updateVersion} is available</span>
            </button>
          )}
          <HealthPill health={health} />
        </div>
      </aside>

      <main id="content" className="main" ref={mainRef}>
        {checkedHealth && !health.reachable && tab !== "settings" && (
          <div className="offline-bar">
            <Callout
              tone="warning"
              icon={TriangleAlert}
              title="Can't reach Gather's local daemon"
              action={
                <Button size="sm" onClick={() => checkHealth().then(setHealth)}>
                  Retry now
                </Button>
              }
            >
              Nothing is lost; your library is still on disk. Gather retries every few seconds.
            </Callout>
          </div>
        )}
        <SectionContext.Provider value={section}>
          <div className="view" key={tab}>
            {tab === "home" && (
              <Overview
                ready={health.ready}
                onNavigate={go}
                onOpenGraph={openGraphFullView}
                onOpenFile={openFile}
                onAddFiles={addFiles}
              />
            )}
            {tab === "library" && (
              <Library selected={libraryFile} onSelect={setLibraryFile} onAddFiles={addFiles} />
            )}
            {tab === "graph" && <Graph onOpenFile={openFile} initialFocus={graphFocus} />}
            {tab === "review" && <ReviewTray />}
            {tab === "clusters" && <Clusters />}
            {tab === "photos" && <Photos />}
            {tab === "contradictions" && <Contradictions />}
            {tab === "entities" && <Entities />}
            {tab === "tuning" && <Tuning />}
            {tab === "settings" && <Settings theme={theme} onTheme={setTheme} />}
          </div>
        </SectionContext.Provider>
      </main>

      <FallbackInput inputRef={uploads.fallbackInput} onFiles={uploads.addFiles} />
      <DropOverlay onFiles={uploads.addFiles} enabled={health.ready} />
      <UploadTray items={uploads.items} onOpen={openFile} onClear={uploads.clear} />
      {paletteOpen && <Palette commands={commands} onClose={() => setPaletteOpen(false)} />}
    </div>
  );
}
