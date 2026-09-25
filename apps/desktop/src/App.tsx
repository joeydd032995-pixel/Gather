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

export default function App() {
  const runtime = useRuntime();
  const settled = runtime.state === "ready" || runtime.state === "unmanaged";
  const [tab, setTab] = useState<Tab>("upload");
  const [updateVersion, setUpdateVersion] = useState<string | null>(null);
  const [health, setHealth] = useState<HealthState>({ reachable: false, ready: false });
  const [dragging, setDragging] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
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

  const onDrop = useCallback(
    (event: React.DragEvent) => {
      event.preventDefault();
      setDragging(false);
      ingest(fromFiles(Array.from(event.dataTransfer.files)));
    },
    [ingest],
  );

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
          {progress
            ? `Uploading ${progress.done + 1} of ${progress.total}…`
            : busy
              ? "Uploading…"
              : "Choose files…"}
        </button>
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
