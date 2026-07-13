import { useCallback, useEffect, useRef, useState } from "react";
import {
  checkHealth,
  uploadFiles,
  type FileResult,
  type HealthState,
} from "./api";
import Contradictions from "./Contradictions";

// Native file picker (Tauri dialog plugin). In a plain browser (vite dev
// outside Tauri) we fall back to a hidden <input type="file">.
const isTauri = "__TAURI_INTERNALS__" in window;

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
  const [tab, setTab] = useState<"upload" | "contradictions">("upload");
  const [health, setHealth] = useState<HealthState>({ reachable: false, ready: false });
  const [dragging, setDragging] = useState(false);
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<FileResult[]>([]);
  const [error, setError] = useState<string | null>(null);
  const fallbackInput = useRef<HTMLInputElement>(null);

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

      <nav className="tabs">
        <button
          className={tab === "upload" ? "tab active" : "tab"}
          onClick={() => setTab("upload")}
        >
          Upload
        </button>
        <button
          className={tab === "contradictions" ? "tab active" : "tab"}
          onClick={() => setTab("contradictions")}
        >
          Contradictions
        </button>
      </nav>

      {tab === "contradictions" && <Contradictions />}

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
