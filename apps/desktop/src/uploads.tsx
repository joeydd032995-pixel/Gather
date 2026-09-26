// Adding files: a drop target covering the whole window, the native (or
// browser) file picker, and a floating tray that follows each upload.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  ArrowUpRight,
  ChevronDown,
  CircleCheck,
  CircleSlash,
  Copy,
  LoaderCircle,
  ShieldCheck,
  Upload,
  X,
} from "lucide-react";
import { uploadFiles } from "./api";
import { kindIcon, kindLabel, plural } from "./kinds";
import { isTauri } from "./native";
import { IconButton } from "./ui";

export const ACCEPT = ".pdf,.md,.markdown,.txt,.png,.jpg,.jpeg,.webp,.tiff,.heic";
const EXTENSIONS = ["pdf", "md", "markdown", "txt", "png", "jpg", "jpeg", "webp", "tiff", "heic"];

/** A file to upload, read only when its turn comes so a large batch never
 *  sits in memory all at once. */
interface UploadSource {
  name: string;
  load: () => Promise<Blob>;
}

export type UploadStatus = "queued" | "uploading" | "accepted" | "deduplicated" | "rejected";

export interface UploadItem {
  key: string;
  name: string;
  status: UploadStatus;
  kind: string | null;
  artifactId: string | null;
  detail: string | null;
  segments: number;
}

function fromFiles(files: File[]): UploadSource[] {
  return files.map((file) => ({ name: file.name, load: async () => file }));
}

async function pickWithNativeDialog(): Promise<UploadSource[]> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const selection = await open({
    multiple: true,
    title: "Add documents or photos to Gather",
    filters: [{ name: "Documents & images", extensions: EXTENSIONS }],
  });
  if (!selection) return [];
  const paths = Array.isArray(selection) ? selection : [selection];
  const { invoke } = await import("@tauri-apps/api/core");
  return paths.map((path) => ({
    name: path.split(/[\\/]/).pop() ?? "unnamed",
    load: async () => new Blob([await invoke<ArrayBuffer>("read_upload_file", { path })]),
  }));
}

let nextKey = 0;

/**
 * The upload queue. One file per request, one at a time: memory stays at one
 * file however large the batch, and a file that fails doesn't stop the rest.
 */
export function useUploads() {
  const [items, setItems] = useState<UploadItem[]>([]);
  const [pickError, setPickError] = useState<string | null>(null);
  const queue = useRef<{ key: string; source: UploadSource }[]>([]);
  const running = useRef(false);
  const fallbackInput = useRef<HTMLInputElement | null>(null);

  const patch = (key: string, change: Partial<UploadItem>) =>
    setItems((prev) => prev.map((i) => (i.key === key ? { ...i, ...change } : i)));

  const drain = useCallback(async () => {
    if (running.current) return;
    running.current = true;
    while (queue.current.length > 0) {
      const { key, source } = queue.current.shift()!;
      patch(key, { status: "uploading" });
      try {
        const blob = await source.load();
        // A browser File keeps its MIME type, which the daemon uses to
        // classify files whose extension doesn't say what they are.
        const file =
          blob instanceof File ? blob : new File([blob], source.name, { type: blob.type });
        const response = await uploadFiles([file]);
        const r = response.files[0];
        patch(key, {
          status: r?.status ?? "rejected",
          kind: r?.kind ?? null,
          artifactId: r?.artifact_id ?? null,
          detail: r?.detail ?? null,
          segments: r?.segments ?? 0,
        });
      } catch (e) {
        patch(key, { status: "rejected", detail: e instanceof Error ? e.message : String(e) });
      }
    }
    running.current = false;
  }, []);

  const ingest = useCallback(
    (sources: UploadSource[]) => {
      if (sources.length === 0) return;
      const added = sources.map((source) => ({ key: `u${nextKey++}`, source }));
      queue.current.push(...added);
      setItems((prev) => [
        ...added.map(({ key, source }) => ({
          key,
          name: source.name,
          status: "queued" as const,
          kind: null,
          artifactId: null,
          detail: null,
          segments: 0,
        })),
        ...prev,
      ]);
      void drain();
    },
    [drain],
  );

  const addFiles = useCallback((files: File[]) => ingest(fromFiles(files)), [ingest]);

  const pick = useCallback(async () => {
    setPickError(null);
    if (isTauri) {
      try {
        ingest(await pickWithNativeDialog());
      } catch (e) {
        setPickError(e instanceof Error ? e.message : String(e));
      }
    } else {
      fallbackInput.current?.click();
    }
  }, [ingest]);

  const clear = useCallback(
    () => setItems((prev) => prev.filter((i) => i.status === "queued" || i.status === "uploading")),
    [],
  );

  const busy = items.some((i) => i.status === "queued" || i.status === "uploading");
  return { items, busy, pick, addFiles, clear, pickError, fallbackInput };
}

/** The hidden <input type="file"> used outside the desktop shell. */
export function FallbackInput({
  inputRef,
  onFiles,
}: {
  inputRef: React.MutableRefObject<HTMLInputElement | null>;
  onFiles: (files: File[]) => void;
}) {
  return (
    <input
      ref={inputRef}
      type="file"
      multiple
      hidden
      accept={ACCEPT}
      onChange={(e) => {
        onFiles(Array.from(e.target.files ?? []));
        e.target.value = "";
      }}
    />
  );
}

/**
 * Files dragged over any part of the window light up a full-window target.
 * Drag events fire for every child crossed, so depth is counted.
 */
export function DropOverlay({
  onFiles,
  enabled,
}: {
  onFiles: (f: File[]) => void;
  enabled: boolean;
}) {
  const [active, setActive] = useState(false);
  const depth = useRef(0);

  useEffect(() => {
    const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes("Files") ?? false;
    const onEnter = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth.current += 1;
      setActive(true);
    };
    const onOver = (e: DragEvent) => {
      if (hasFiles(e)) e.preventDefault();
    };
    const onLeave = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      depth.current = Math.max(0, depth.current - 1);
      if (depth.current === 0) setActive(false);
    };
    const onDrop = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth.current = 0;
      setActive(false);
      if (enabled) onFiles(Array.from(e.dataTransfer?.files ?? []));
    };
    window.addEventListener("dragenter", onEnter);
    window.addEventListener("dragover", onOver);
    window.addEventListener("dragleave", onLeave);
    window.addEventListener("drop", onDrop);
    return () => {
      window.removeEventListener("dragenter", onEnter);
      window.removeEventListener("dragover", onOver);
      window.removeEventListener("dragleave", onLeave);
      window.removeEventListener("drop", onDrop);
    };
  }, [enabled, onFiles]);

  if (!active) return null;
  return (
    <div className="drop-overlay" role="presentation">
      <div className="drop-card">
        <div className="drop-art" aria-hidden>
          <span className="drop-wave w1" />
          <span className="drop-wave w2" />
          <span className="drop-core">
            <Upload />
          </span>
        </div>
        <p className="drop-title">{enabled ? "Drop to add to Gather" : "Gather isn't ready yet"}</p>
        <p className="drop-sub">
          <ShieldCheck aria-hidden />
          Read on this computer. Nothing is uploaded anywhere.
        </p>
      </div>
    </div>
  );
}

const STATUS: Record<UploadStatus, { label: string; icon: typeof CircleCheck; tone: string }> = {
  queued: { label: "Waiting", icon: LoaderCircle, tone: "neutral" },
  uploading: { label: "Adding", icon: LoaderCircle, tone: "accent" },
  accepted: { label: "Added", icon: CircleCheck, tone: "success" },
  deduplicated: { label: "Already added", icon: Copy, tone: "warning" },
  rejected: { label: "Not added", icon: CircleSlash, tone: "danger" },
};

/** Bottom-right card that follows the current batch of uploads. */
export function UploadTray({
  items,
  onOpen,
  onClear,
}: {
  items: UploadItem[];
  onOpen: (artifactId: string) => void;
  onClear: () => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  if (items.length === 0) return null;
  const done = items.filter((i) => i.status !== "queued" && i.status !== "uploading").length;
  const busy = done < items.length;
  const pct = (done / items.length) * 100;
  return (
    <section className="tray" aria-label="Uploads" aria-live="polite">
      <header className="tray-head">
        <span className={busy ? "tray-status busy" : "tray-status"} aria-hidden>
          {busy ? <LoaderCircle className="spin" /> : <CircleCheck />}
        </span>
        <div className="tray-title">
          <strong>
            {busy
              ? `Adding ${done + 1} of ${items.length}…`
              : `${plural(items.filter((i) => i.status === "accepted").length, "file")} added`}
          </strong>
          <span className="hint">
            {busy ? "Reading happens on this computer" : "Gather is reading them in the background"}
          </span>
        </div>
        <IconButton
          icon={ChevronDown}
          label={collapsed ? "Show uploads" : "Hide uploads"}
          size="sm"
          tip="top"
          className={collapsed ? "tray-toggle collapsed" : "tray-toggle"}
          onClick={() => setCollapsed((c) => !c)}
        />
        {!busy && <IconButton icon={X} label="Dismiss" size="sm" tip="top" onClick={onClear} />}
      </header>
      <div className="tray-progress">
        <span style={{ width: `${pct}%` }} />
      </div>
      {!collapsed && (
        <ul className="tray-list">
          {items.map((item) => {
            const s = STATUS[item.status];
            const Icon = kindIcon(item.kind);
            const StatusIcon = s.icon;
            const spinning = item.status === "uploading" || item.status === "queued";
            return (
              <li key={item.key} className="tray-item">
                <span className="tray-file" aria-hidden>
                  <Icon />
                </span>
                <span className="tray-text">
                  <span className="tray-name">{item.name}</span>
                  <span className={`tray-meta tone-${s.tone}`}>
                    <StatusIcon className={spinning && item.status === "uploading" ? "spin" : ""} />
                    {s.label}
                    {item.kind && <span className="dot-sep">{kindLabel(item.kind)}</span>}
                    {item.detail && <span className="dot-sep tray-detail">{item.detail}</span>}
                  </span>
                </span>
                {item.artifactId && (
                  <IconButton
                    icon={ArrowUpRight}
                    label={`Open ${item.name} in the Library`}
                    size="sm"
                    tip="left"
                    onClick={() => onOpen(item.artifactId!)}
                  />
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
