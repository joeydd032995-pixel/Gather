import { useCallback, useEffect, useRef, useState } from "react";
import {
  checkHealth,
  setApiToken,
  uploadFiles,
  type FileResult,
  type HealthState,
} from "./api";
import Clusters from "./Clusters";
import Contradictions from "./Contradictions";
import Entities from "./Entities";
import Photos from "./Photos";
import { useRuntime } from "./hooks/useRuntime";
import { checkForUpdate, getApiToken, getUpdateSettings, isTauri } from "./native";
import ReviewTray from "./ReviewTray";
import Settings from "./Settings";
import Tuning from "./Tuning";

type Tab =
  | "upload"
  | "review"
  | "clusters"
  | "photos"
  | "contradictions"
  | "entities"
  | "tuning"
  | "settings";

const TABS: { id: Tab; label: string }[] = [
  { id: "upload", label: "Upload" },
  { id: "review", label: "Review" },
  { id: "clusters", label: "Groups" },
  { id: "photos", label: "Photos" },
  { id: "contradictions", label: "Contradictions" },
  { id: "entities", label: "Entities" },
  { id: "tuning", label: "Tuning" },
  { id: "settings", label: "Settings" },
];

// Native file picker (Tauri dialog plugin). In a plain browser (vite dev
// outside Tauri) we fall back to a hidden <input type="file">.

async function pickWithNativeDialog(): Promise<File[]> {
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
  const files: File[] = [];
  for (const path of paths) {
    const bytes = await invoke<number[]>("read_upload_file", { path });
    const name = path.split(/[\\/]/).pop() ?? "unnamed";
    files.push(new File([new Uint8Array(bytes)], name));
  }
  return files;
}

export default function App() {
  const runtime = useRuntime();
  const settled = runtime.state === "ready" || runtime.state === "unmanaged";
  const [tab, setTab] = useState<Tab>("upload");
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [health, setHealth] = useState<HealthState>({ reachable: false, ready: false });
  const [dragging, setDragging] = useState(false);
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<FileResult[]>([]);
  const [error, setError] = useState<string | null>(null);
  const fallbackInput = useRef<HTMLInputElement>(null);

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
      if (!cancelled) setHealth(h);
    };
    poll();
    const timer = setInterval(poll, 5000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  const ingest = useCallback(async (files: File[]) => {
    if (files.length === 0) return;
    setBusy(true);
    setError(null);
    try {
      const response = await uploadFiles(files);
      setResults((prev) => [...response.files, ...prev]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  const onDrop = useCallback(
    (event: React.DragEvent) => {
      event.preventDefault();
      setDragging(false);
      ingest(Array.from(event.dataTransfer.files));
    },
    [ingest],
  );

  const onPick = useCallback(async () => {
    if (isTauri) {
      ingest(await pickWithNativeDialog());
    } else {
      fallbackInput.current?.click();
    }
  }, [ingest]);

  if (runtime.state === "starting" || runtime.state === "failed") {
    return (
      <main className="app">
        <header>
          <h1>Gather</h1>
        </header>
        {runtime.state === "starting" ? (
          <p className="hint">{runtime.step}…</p>
        ) : (
          <>
            <p className="error">{runtime.message}</p>
            {runtime.log_dir && <p className="hint">Logs: {runtime.log_dir}</p>}
          </>
        )}
      </main>
    );
  }

  return (
    <main className="app">
      <header>
        <h1>Gather</h1>
        <span
          className={`health ${health.ready ? "ok" : health.reachable ? "warn" : "down"}`}
          title={health.ready ? "daemon ready" : health.reachable ? "daemon up, database not ready" : "daemon unreachable"}
        >
          {health.ready ? "● local daemon ready" : health.reachable ? "● database not ready" : "○ daemon offline"}
        </span>
      </header>

      {updateVersion && (
        <p className="hint">
          Gather {updateVersion} is available.{" "}
          <button className="link-button" onClick={() => setTab("settings")}>
            Update in Settings
          </button>
        </p>
      )}

      <nav className="tabs">
        {TABS.map((t) => (
          <button
            key={t.id}
            className={tab === t.id ? "tab active" : "tab"}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </nav>

      {tab === "review" && <ReviewTray />}

      {tab === "clusters" && <Clusters />}

      {tab === "photos" && <Photos />}

      {tab === "tuning" && <Tuning />}

      {tab === "settings" && <Settings />}

      {tab === "contradictions" && <Contradictions />}

      {tab === "entities" && <Entities />}

      {tab === "upload" && (
      <>
      <section
        className={`dropzone ${dragging ? "dragging" : ""}`}
        onDragOver={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={onDrop}
      >
        <p>Drag &amp; drop PDFs, markdown, text files, photos or screenshots here</p>
        <button onClick={onPick} disabled={busy || !health.ready}>
          {busy ? "Uploading…" : "Choose files…"}
        </button>
        <input
          ref={fallbackInput}
          type="file"
          multiple
          hidden
          accept=".pdf,.md,.markdown,.txt,.png,.jpg,.jpeg,.webp,.tiff,.heic"
          onChange={(e) => {
            ingest(Array.from(e.target.files ?? []));
            e.target.value = "";
          }}
        />
      </section>

      {error && <p className="error">{error}</p>}
      </>
      )}

      {tab === "upload" && results.length > 0 && (
        <table className="results">
          <thead>
            <tr>
              <th>File</th>
              <th>Kind</th>
              <th>Status</th>
              <th>Segments</th>
            </tr>
          </thead>
          <tbody>
            {results.map((r, i) => (
              <tr key={`${r.artifact_id ?? r.filename}-${i}`}>
                <td>{r.filename}</td>
                <td>{r.kind ?? "—"}</td>
                <td className={`status-${r.status}`}>
                  {r.status}
                  {r.detail ? ` — ${r.detail}` : ""}
                </td>
                <td>{r.segments}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </main>
  );
}
