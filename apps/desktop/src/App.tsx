import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowUpRight,
  Command as CommandIcon,
  FilePlus,
  LoaderCircle,
  Monitor,
  Moon,
  Search,
  Sun,
  TriangleAlert,
} from "lucide-react";
import { checkHealth, setApiToken, uploadFiles, type FileResult, type HealthState } from "./api";
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
import { NAV, NAV_ITEMS, WIDE_VIEWS, type Tab } from "./nav";
import Palette, { type Command } from "./Palette";
import Photos from "./Photos";
import ReviewTray from "./ReviewTray";
import Settings from "./Settings";
import Tuning from "./Tuning";
import { Button, Callout, Kbd, SectionContext } from "./ui";
import Upload from "./Upload";

// Native file picker (Tauri dialog plugin). In a plain browser (vite dev
// outside Tauri) we fall back to a hidden <input type="file">.

/** A file to upload, read only when its turn comes so a large batch never
 *  sits in memory all at once. */
interface UploadSource {
  name: string;
  load: () => Promise<Blob>;
}

function fromFiles(files: File[]): UploadSource[] {
  return files.map((file) => ({ name: file.name, load: async () => file }));
}

async function pickWithNativeDialog(): Promise<UploadSource[]> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const selection = await open({
    multiple: true,
    title: "Add documents or photos to Gather",
    filters: [
      {
        name: "Documents & images",
        extensions: ["pdf", "md", "markdown", "txt", "png", "jpg", "jpeg", "webp", "tiff", "heic"],
      },
    ],
  });
  if (!selection) return [];
  const paths = Array.isArray(selection) ? selection : [selection];
  const { invoke } = await import("@tauri-apps/api/core");
  return paths.map((path) => ({
    name: path.split(/[\\/]/).pop() ?? "unnamed",
    load: async () => new Blob([await invoke<ArrayBuffer>("read_upload_file", { path })]),
  }));
}

const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
const MOD = isMac ? "⌘" : "Ctrl";

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
      <div className="splash-card">
        <Logo size={48} />
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
  const label = health.ready ? "Ready" : health.reachable ? "Database starting" : "Daemon offline";
  const detail = health.ready
    ? "The local daemon and database are running. Everything stays on this computer."
    : health.reachable
      ? "The daemon is up but its database isn't ready yet."
      : "Gather's local daemon isn't reachable. Retrying every 5 seconds.";
  return (
    <div className={`health health-${state}`} title={detail} role="status" aria-live="polite">
      <span className="health-dot" aria-hidden />
      <span className="health-text">
        <span className="health-label">{label}</span>
        <span className="health-sub">Local · offline</span>
      </span>
    </div>
  );
}

export default function App() {
  const runtime = useRuntime();
  const settled = runtime.state === "ready" || runtime.state === "unmanaged";
  const [tab, setTab] = useState<Tab>("upload");
  /** The file open in the Library; the graph and upload results set it too. */
  const [libraryFile, setLibraryFile] = useState<string | null>(null);
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [health, setHealth] = useState<HealthState>({ reachable: false, ready: false });
  const [checkedHealth, setCheckedHealth] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [results, setResults] = useState<FileResult[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [theme, setTheme] = useTheme();
  const fallbackInput = useRef<HTMLInputElement>(null);
  const mainRef = useRef<HTMLElement>(null);
  const navigated = useRef(false);
  const counts = useAttention(health.ready, tab);

  const go = useCallback((next: Tab) => {
    navigated.current = true;
    setTab(next);
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

  // A view change moves focus to its heading, so screen readers announce it
  // and keyboard users start at the top of the new view.
  useEffect(() => {
    if (!navigated.current) return;
    mainRef.current?.scrollTo({ top: 0 });
    mainRef.current
      ?.querySelector<HTMLElement>("[data-page-title]")
      ?.focus({ preventScroll: true });
  }, [tab]);

  // Ctrl/⌘+K opens the palette from anywhere.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Lets the visual checks in development drive navigation.
  useEffect(() => {
    if (!import.meta.env.DEV) return;
    (window as unknown as { __gatherNav?: (t: Tab) => void }).__gatherNav = go;
  }, [go]);

  // One file per request, one at a time: memory stays at one file however
  // large the batch, and a file that fails doesn't stop the rest.
  const ingest = useCallback(async (sources: UploadSource[]) => {
    if (sources.length === 0) return;
    setBusy(true);
    setError(null);
    for (const [i, source] of sources.entries()) {
      setProgress({ done: i, total: sources.length });
      try {
        const blob = await source.load();
        // A browser File keeps its MIME type, which the daemon uses to
        // classify files whose extension doesn't say what they are.
        const file =
          blob instanceof File ? blob : new File([blob], source.name, { type: blob.type });
        const response = await uploadFiles([file]);
        setResults((prev) => [...response.files, ...prev]);
      } catch (e) {
        const detail = e instanceof Error ? e.message : String(e);
        setResults((prev) => [
          {
            filename: source.name,
            kind: null,
            artifact_id: null,
            deduplicated: false,
            status: "rejected",
            detail,
            segments: 0,
          },
          ...prev,
        ]);
      }
    }
    setProgress(null);
    setBusy(false);
  }, []);

  const onPick = useCallback(async () => {
    if (isTauri) {
      try {
        await ingest(await pickWithNativeDialog());
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    } else {
      fallbackInput.current?.click();
    }
  }, [ingest]);

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
        label: "Choose files to add…",
        icon: FilePlus,
        run: () => {
          go("upload");
          if (health.ready && !busy) onPick();
        },
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
    [busy, go, health.ready, onPick, setTheme, theme],
  );

  if (runtime.state === "starting") return <Splash step={runtime.step} />;
  if (runtime.state === "failed") {
    return <Splash error={runtime.message} logDir={runtime.log_dir} />;
  }

  const ThemeIcon = THEME_ICONS[theme];
  const wide = WIDE_VIEWS.has(tab);
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
        </div>

        <button
          type="button"
          className="jump"
          onClick={() => setPaletteOpen(true)}
          aria-label={`Search or jump to… (${MOD}+K)`}
          title={`Search or jump to… (${MOD}+K)`}
        >
          <Search aria-hidden />
          <span className="jump-label">Jump to…</span>
          <span className="jump-kbd" aria-hidden>
            <Kbd>{isMac ? <CommandIcon size={10} /> : "Ctrl"}</Kbd>
            <Kbd>K</Kbd>
          </span>
        </button>

        <nav className="nav" aria-label="Main">
          {NAV.map((group) => (
            <div className="nav-group" key={group.label}>
              <div className="nav-group-label">{group.label}</div>
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
                        title={item.label}
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
          <div className="sidebar-foot-row">
            <HealthPill health={health} />
            <button
              type="button"
              className="theme-toggle"
              onClick={() => setTheme(THEME_NEXT[theme])}
              aria-label={`Appearance: ${theme}. Switch to ${THEME_NEXT[theme]}.`}
              title={`Appearance: ${theme}`}
            >
              <ThemeIcon aria-hidden />
            </button>
          </div>
        </div>
      </aside>

      <main id="content" className="main" ref={mainRef}>
        <SectionContext.Provider value={section}>
          <div className={wide ? "view view-wide" : "view"} key={tab}>
            {checkedHealth && !health.reachable && tab !== "settings" && (
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
            )}

            {tab === "upload" && (
              <Upload
                ready={health.ready}
                busy={busy}
                progress={progress}
                results={results}
                error={error}
                onPick={onPick}
                onDropFiles={(files) => ingest(fromFiles(files))}
                onOpenFile={openFile}
                onClear={() => setResults([])}
              />
            )}
            {tab === "library" && <Library selected={libraryFile} onSelect={setLibraryFile} />}
            {tab === "graph" && <Graph onOpenFile={openFile} />}
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

      <input
        ref={fallbackInput}
        type="file"
        multiple
        hidden
        accept=".pdf,.md,.markdown,.txt,.png,.jpg,.jpeg,.webp,.tiff,.heic"
        onChange={(e) => {
          ingest(fromFiles(Array.from(e.target.files ?? [])));
          e.target.value = "";
        }}
      />

      {paletteOpen && <Palette commands={commands} onClose={() => setPaletteOpen(false)} />}
    </div>
  );
}
