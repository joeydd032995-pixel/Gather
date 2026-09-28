// Adding files: a drop target covering the whole window, the native (or
// browser) file picker, and a floating tray that follows each upload.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  Archive,
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
import {
  createProject,
  importProjectZip,
  describeProject,
  uploadFiles,
  uploadProjectFile,
  type ProjectFileResult,
} from "./api";
import { kindIcon, kindLabel, plural } from "./kinds";
import { isTauri } from "./native";
import { IconButton } from "./ui";

/** What "Add files" offers: every kind Gather reads (the daemon's lists).
 *  Inside a project folder every file is taken, whatever its kind. */
const EXTENSIONS = [
  "pdf",
  "md",
  "markdown",
  "docx",
  "docm",
  "xlsx",
  "xlsm",
  "xlsb",
  "xls",
  "ods",
  "html",
  "htm",
  "xhtml",
  "txt",
  "text",
  "log",
  "csv",
  "tsv",
  "json",
  "jsonl",
  "ndjson",
  "yaml",
  "yml",
  "toml",
  "ini",
  "cfg",
  "conf",
  "properties",
  "xml",
  "svg",
  "css",
  "scss",
  "less",
  "sql",
  "graphql",
  "gql",
  "proto",
  "sh",
  "bash",
  "zsh",
  "fish",
  "ps1",
  "bat",
  "cmd",
  "py",
  "pyi",
  "ipynb",
  "rs",
  "go",
  "rb",
  "php",
  "pl",
  "lua",
  "r",
  "jl",
  "js",
  "mjs",
  "cjs",
  "ts",
  "mts",
  "cts",
  "tsx",
  "jsx",
  "vue",
  "svelte",
  "java",
  "kt",
  "kts",
  "scala",
  "groovy",
  "gradle",
  "swift",
  "m",
  "mm",
  "c",
  "h",
  "cc",
  "cpp",
  "cxx",
  "hpp",
  "hh",
  "cs",
  "fs",
  "vb",
  "dart",
  "ex",
  "exs",
  "erl",
  "hs",
  "clj",
  "elm",
  "zig",
  "nim",
  "tf",
  "hcl",
  "rst",
  "adoc",
  "asciidoc",
  "org",
  "tex",
  "bib",
  "srt",
  "vtt",
  "png",
  "jpg",
  "jpeg",
  "webp",
  "tiff",
  "heic",
  "zip",
];
export const ACCEPT = EXTENSIONS.map((e) => `.${e}`).join(",");

/** Folders a project upload leaves out whole: version-control history,
 *  installed dependencies and tool caches. Their files aren't sent; the
 *  folders are reported so the project's tree still shows them. The daemon
 *  applies the same list. */
const LEFT_OUT_DIRS = new Set([
  ".git",
  ".hg",
  ".svn",
  "node_modules",
  "bower_components",
  ".venv",
  "venv",
  "__pycache__",
  ".tox",
  ".mypy_cache",
  ".pytest_cache",
  ".ruff_cache",
  ".gradle",
  ".next",
  ".nuxt",
  ".terraform",
]);
/** Files that usually hold keys or passwords. Their bytes are never read
 *  or sent; only their paths, so the project lists them as skipped. The
 *  daemon applies the same rules. */
const SECRET_NAMES = new Set([
  "id_rsa",
  "id_dsa",
  "id_ecdsa",
  "id_ed25519",
  ".npmrc",
  ".pypirc",
  ".netrc",
  ".pgpass",
  ".htpasswd",
  "credentials",
  "credentials.json",
  "secrets.json",
  "secrets.yaml",
  "secrets.yml",
]);
const SECRET_EXTENSIONS = new Set([
  "pem",
  "key",
  "p12",
  "pfx",
  "jks",
  "keystore",
  "kdbx",
  "ppk",
  "asc",
  "gpg",
]);

/** Operating-system and archive-tool clutter, left out without a trace. */
const CLUTTER_DIRS = new Set(["__macosx"]);
const CLUTTER_FILES = new Set([".ds_store", "thumbs.db", "desktop.ini", ".localized"]);
/** Most files added from one folder, as the desktop app's folder listing allows. */
const MAX_PROJECT_FILES = 20_000;

/** What a project upload does with a path inside the project: send it,
 *  drop it as clutter, leave out the folder it sits in, or withhold it
 *  unread (secrets). */
type Screen =
  | { action: "send" }
  | { action: "drop" }
  | { action: "leave-out"; folder: string }
  | { action: "withhold" };

function screenPath(path: string): Screen {
  const parts = path.split("/");
  const lower = parts.map((p) => p.toLowerCase());
  for (let i = 0; i < parts.length - 1; i++) {
    if (CLUTTER_DIRS.has(lower[i])) return { action: "drop" };
    if (LEFT_OUT_DIRS.has(lower[i])) {
      return { action: "leave-out", folder: parts.slice(0, i + 1).join("/") };
    }
  }
  const name = lower[lower.length - 1];
  if (CLUTTER_FILES.has(name) || name.startsWith("._")) return { action: "drop" };
  const ext = name.includes(".") ? name.slice(name.lastIndexOf(".") + 1) : "";
  if (name.startsWith(".env") || SECRET_NAMES.has(name) || SECRET_EXTENSIONS.has(ext)) {
    return { action: "withhold" };
  }
  return { action: "send" };
}

const isZip = (name: string) => name.toLowerCase().endsWith(".zip");

/** A file to upload, read only when its turn comes so a large batch never
 *  sits in memory all at once. */
interface UploadSource {
  name: string;
  load: () => Promise<Blob>;
}

/** A file inside a project folder, by its path in the project. */
export interface ProjectEntry {
  path: string;
  source: UploadSource;
}

type Target =
  | { type: "file" }
  | { type: "zip" }
  | { type: "project"; projectId: string; project: string; path: string };

export type UploadStatus =
  | "queued"
  | "uploading"
  | "accepted"
  | "deduplicated"
  | "stored"
  | "skipped"
  | "rejected";

export interface UploadItem {
  key: string;
  name: string;
  status: UploadStatus;
  kind: string | null;
  artifactId: string | null;
  /** The project the file belongs to (or a .zip became). */
  project: string | null;
  projectId: string | null;
  detail: string | null;
  segments: number;
}

function fromFiles(files: File[]): UploadSource[] {
  return files.map((file) => ({ name: file.name, load: async () => file }));
}

async function readNative(path: string): Promise<Blob> {
  const { invoke } = await import("@tauri-apps/api/core");
  return new Blob([await invoke<ArrayBuffer>("read_upload_file", { path })]);
}

async function pickWithNativeDialog(): Promise<UploadSource[]> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const selection = await open({
    multiple: true,
    title: "Add files to Gather",
    filters: [{ name: "Documents, images and .zip projects", extensions: EXTENSIONS }],
  });
  if (!selection) return [];
  const paths = Array.isArray(selection) ? selection : [selection];
  return paths.map((path) => ({
    name: path.split(/[\\/]/).pop() ?? "unnamed",
    load: () => readNative(path),
  }));
}

interface FolderListing {
  name: string;
  files: { path: string; abs: string; size: number }[];
  /** Folders left out whole, by their path in the project. */
  left_out: string[];
  /** Folders with nothing in them, so the tree keeps them. */
  empty_folders: string[];
  truncated: boolean;
}

/** A folder's files to send, and what it held that isn't sent. */
interface PickedProject {
  entries: ProjectEntry[];
  /** Folders left out whole, by their path in the project. */
  leftOut: string[];
  /** Secret-looking files, by path: never read, listed as skipped. */
  withheld: string[];
  /** Empty folders, so the project's tree keeps them. */
  folders: string[];
}

const emptyPick = (): PickedProject => ({ entries: [], leftOut: [], withheld: [], folders: [] });

/** Sort one file of a picked folder into `pick`. `source` is only called
 *  for a file that will be sent. Returns whether anything was recorded. */
function pickFile(pick: PickedProject, path: string, source: () => UploadSource): boolean {
  const screen = screenPath(path);
  switch (screen.action) {
    case "send":
      pick.entries.push({ path, source: source() });
      return true;
    case "withhold":
      pick.withheld.push(path);
      return true;
    case "leave-out":
      if (!pick.leftOut.includes(screen.folder)) pick.leftOut.push(screen.folder);
      return true;
    case "drop":
      return false;
  }
}

async function pickFolderWithNativeDialog(): Promise<FolderListing | null> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const selection = await open({ directory: true, title: "Add a project folder to Gather" });
  if (!selection || Array.isArray(selection)) return null;
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<FolderListing>("list_project_folder", { path: selection });
}

/** Everything under a dropped folder, with paths inside that folder. */
async function readDroppedFolder(dir: FileSystemDirectoryEntry): Promise<PickedProject> {
  const pick = emptyPick();
  const prefix = `${dir.fullPath}/`;
  const full = () => pick.entries.length > MAX_PROJECT_FILES;
  const relative = (entry: FileSystemEntry) =>
    entry.fullPath.startsWith(prefix) ? entry.fullPath.slice(prefix.length) : entry.name;
  /** Walks `d`; returns whether anything under it was recorded. */
  const walk = async (d: FileSystemDirectoryEntry): Promise<boolean> => {
    let recorded = false;
    const reader = d.createReader();
    // readEntries returns the folder in batches until an empty one.
    for (;;) {
      // One past the limit is enough for addProject to say it was reached.
      if (full()) return true;
      const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
        reader.readEntries(resolve, reject),
      );
      if (batch.length === 0) break;
      for (const entry of batch) {
        if (full()) return true;
        const path = relative(entry);
        if (entry.isDirectory) {
          // Screened as if it held a file, so its own name counts.
          const screen = screenPath(`${path}/-`);
          if (screen.action === "leave-out") {
            if (!pick.leftOut.includes(screen.folder)) pick.leftOut.push(screen.folder);
            recorded = true;
          } else if (screen.action !== "drop") {
            if (!(await walk(entry as FileSystemDirectoryEntry))) pick.folders.push(path);
            recorded = true;
          }
        } else {
          const fileEntry = entry as FileSystemFileEntry;
          recorded =
            pickFile(pick, path, () => ({
              name: entry.name,
              load: () => new Promise<File>((resolve, reject) => fileEntry.file(resolve, reject)),
            })) || recorded;
        }
      }
    }
    return recorded;
  };
  await walk(dir);
  pick.entries.sort((a, b) => (a.path < b.path ? -1 : 1));
  pick.leftOut.sort();
  return pick;
}

/** Files from an <input webkitdirectory>, grouped by the folder picked.
 *  (Browsers don't list empty folders here.) */
function groupPickedFolder(files: File[]): Map<string, PickedProject> {
  const projects = new Map<string, PickedProject>();
  for (const file of files) {
    const rel = file.webkitRelativePath || file.name;
    const [root, ...rest] = rel.split("/");
    const path = rest.length > 0 ? rest.join("/") : root;
    const project = projects.get(root) ?? emptyPick();
    projects.set(root, project);
    pickFile(project, path, () => ({ name: file.name, load: async () => file }));
  }
  return projects;
}

const PROJECT_STATUS: Record<ProjectFileResult["status"], UploadStatus> = {
  ingested: "accepted",
  deduplicated: "deduplicated",
  stored: "stored",
  skipped: "skipped",
  left_out: "skipped",
  ignored: "skipped",
  failed: "rejected",
};

/** "3 files read, 1 kept as it is, 2 skipped" for a .zip unpacked in place. */
function unpackedSummary(files: ProjectFileResult[], stopped: string | null): string {
  const count = (status: ProjectFileResult["status"]) =>
    files.filter((f) => f.status === status).length;
  const parts = [`${plural(count("ingested"), "file")} read`];
  if (count("deduplicated") > 0) parts.push(`${count("deduplicated")} already in Gather`);
  if (count("stored") > 0) parts.push(`${count("stored")} kept as they are`);
  const skipped = count("skipped") + count("failed");
  if (skipped > 0) parts.push(`${skipped} skipped`);
  if (count("left_out") > 0) parts.push(`${plural(count("left_out"), "folder")} left out`);
  if (stopped) parts.push(stopped);
  return parts.join(", ");
}

let nextKey = 0;

/**
 * The upload queue. One file per request, one at a time: memory stays at one
 * file however large the batch, and a file that fails doesn't stop the rest.
 * Folders become projects: the project is created first, then its files are
 * sent with their paths. A .zip is unpacked by the daemon into a project.
 */
export function useUploads() {
  const [items, setItems] = useState<UploadItem[]>([]);
  const [pickError, setPickError] = useState<string | null>(null);
  /** Bumped whenever an upload finishes, so views can refresh. */
  const [version, setVersion] = useState(0);
  const queue = useRef<{ key: string; source: UploadSource; target: Target }[]>([]);
  const running = useRef(false);
  const fallbackInput = useRef<HTMLInputElement | null>(null);
  const folderInput = useRef<HTMLInputElement | null>(null);
  const zipInput = useRef<HTMLInputElement | null>(null);

  const patch = (key: string, change: Partial<UploadItem>) =>
    setItems((prev) => prev.map((i) => (i.key === key ? { ...i, ...change } : i)));

  const drain = useCallback(async () => {
    if (running.current) return;
    running.current = true;
    while (queue.current.length > 0) {
      const { key, source, target } = queue.current.shift()!;
      patch(key, { status: "uploading" });
      try {
        const blob = await source.load();
        // A browser File keeps its MIME type, which the daemon uses to
        // classify files whose extension doesn't say what they are.
        const file =
          blob instanceof File ? blob : new File([blob], source.name, { type: blob.type });
        if (target.type === "project") {
          const response = await uploadProjectFile(target.projectId, target.path, file);
          const [r] = response.files;
          const unpacked = response.files.length > 1 || (r && r.path !== target.path);
          if (!r || unpacked) {
            // A .zip unpacked where it sat: one line for everything in it.
            patch(key, {
              status: "accepted",
              kind: null,
              detail: `Unpacked: ${unpackedSummary(response.files, response.stopped)}`,
            });
          } else {
            patch(key, {
              status: PROJECT_STATUS[r.status] ?? "rejected",
              kind: r.kind,
              artifactId: r.artifact_id,
              detail: r.status === "ignored" ? "Left out (system clutter)" : r.detail,
              segments: r.segments,
            });
          }
        } else if (target.type === "zip") {
          const report = await importProjectZip(file);
          const p = report.project;
          const parts = [`${plural(p.ingested, "file")} read`];
          if (p.deduplicated > 0) parts.push(`${p.deduplicated} already in Gather`);
          if (p.stored > 0) parts.push(`${p.stored} kept as they are`);
          if (p.skipped + p.failed > 0) parts.push(`${p.skipped + p.failed} skipped`);
          if (p.left_out > 0) parts.push(`${plural(p.left_out, "folder")} left out`);
          if (report.stopped) parts.push(report.stopped);
          patch(key, {
            status: "accepted",
            kind: null,
            project: p.name,
            projectId: p.id,
            detail: parts.join(", "),
          });
        } else {
          const response = await uploadFiles([file]);
          const r = response.files[0];
          patch(key, {
            status: r?.status ?? "rejected",
            kind: r?.kind ?? null,
            artifactId: r?.artifact_id ?? null,
            detail: r?.detail ?? null,
            segments: r?.segments ?? 0,
          });
        }
      } catch (e) {
        patch(key, { status: "rejected", detail: e instanceof Error ? e.message : String(e) });
      }
      setVersion((v) => v + 1);
    }
    running.current = false;
  }, []);

  const enqueue = useCallback(
    (jobs: { source: UploadSource; target: Target; name: string }[]) => {
      if (jobs.length === 0) return;
      const added = jobs.map((job) => ({ key: `u${nextKey++}`, ...job }));
      queue.current.push(...added.map(({ key, source, target }) => ({ key, source, target })));
      setItems((prev) => [
        ...added.map(({ key, name, target }) => ({
          key,
          name,
          status: "queued" as const,
          kind: null,
          artifactId: null,
          project: target.type === "project" ? target.project : null,
          projectId: target.type === "project" ? target.projectId : null,
          detail: null,
          segments: 0,
        })),
        ...prev,
      ]);
      void drain();
    },
    [drain],
  );

  /** Loose files; a .zip among them becomes a project. */
  const ingest = useCallback(
    (sources: UploadSource[]) =>
      enqueue(
        sources.map((source) => ({
          source,
          name: source.name,
          target: isZip(source.name) ? { type: "zip" as const } : { type: "file" as const },
        })),
      ),
    [enqueue],
  );

  /** A folder: create the project, then queue its files with their paths. */
  const addProject = useCallback(
    async (name: string, pick: PickedProject) => {
      let { entries } = pick;
      const { leftOut, withheld, folders } = pick;
      if (entries.length + leftOut.length + withheld.length + folders.length === 0) {
        setPickError(`“${name}” has no files Gather can add.`);
        return;
      }
      if (entries.length > MAX_PROJECT_FILES) {
        entries = entries.slice(0, MAX_PROJECT_FILES);
        setPickError(
          `“${name}” has more than ${MAX_PROJECT_FILES.toLocaleString()} files; only those were added.`,
        );
      }
      try {
        const project = await createProject(name);
        if (leftOut.length + withheld.length + folders.length > 0) {
          // Shown in the tray and the project's tree, so nothing a folder
          // held disappears without a word.
          const results = await describeProject(project.id, { leftOut, withheld, folders });
          setItems((prev) => [
            ...results.map((r) => ({
              key: `u${nextKey++}`,
              name: r.path,
              status: PROJECT_STATUS[r.status] ?? "skipped",
              kind: null,
              artifactId: null,
              project: project.name,
              projectId: project.id,
              detail: r.status === "left_out" ? `Folder left out: ${r.detail}` : r.detail,
              segments: 0,
            })),
            ...prev,
          ]);
        }
        enqueue(
          entries.map(({ path, source }) => ({
            source,
            name: path,
            target: { type: "project", projectId: project.id, project: project.name, path },
          })),
        );
        setVersion((v) => v + 1);
      } catch (e) {
        setPickError(e instanceof Error ? e.message : String(e));
      }
    },
    [enqueue],
  );

  const addFiles = useCallback((files: File[]) => ingest(fromFiles(files)), [ingest]);

  /** What a drop held: folders become projects, files are added as usual. */
  const addDropped = useCallback(
    async (folders: FileSystemDirectoryEntry[], files: File[]) => {
      addFiles(files);
      for (const folder of folders) {
        try {
          await addProject(folder.name, await readDroppedFolder(folder));
        } catch (e) {
          setPickError(e instanceof Error ? e.message : String(e));
        }
      }
    },
    [addFiles, addProject],
  );

  /** Files picked through <input webkitdirectory>. */
  const addPickedFolder = useCallback(
    (files: File[]) => {
      for (const [root, entries] of groupPickedFolder(files)) void addProject(root, entries);
    },
    [addProject],
  );

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

  /** Pick a .zip to unpack as a project. */
  const pickZip = useCallback(async () => {
    setPickError(null);
    if (!isTauri) {
      zipInput.current?.click();
      return;
    }
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const selection = await open({
        multiple: true,
        title: "Import a project .zip",
        filters: [{ name: "Zip archives", extensions: ["zip"] }],
      });
      if (!selection) return;
      const paths = Array.isArray(selection) ? selection : [selection];
      ingest(
        paths.map((path) => ({
          name: path.split(/[\\/]/).pop() ?? "project.zip",
          load: () => readNative(path),
        })),
      );
    } catch (e) {
      setPickError(e instanceof Error ? e.message : String(e));
    }
  }, [ingest]);

  const pickFolder = useCallback(async () => {
    setPickError(null);
    if (!isTauri) {
      folderInput.current?.click();
      return;
    }
    try {
      const listing = await pickFolderWithNativeDialog();
      if (!listing) return;
      if (listing.truncated) {
        setPickError(
          `“${listing.name}” has more than ${listing.files.length.toLocaleString()} files; only those were added.`,
        );
      }
      const pick = emptyPick();
      for (const f of listing.files) {
        pickFile(pick, f.path, () => ({
          name: f.path.split("/").pop() ?? f.path,
          load: () => readNative(f.abs),
        }));
      }
      pick.leftOut.push(...listing.left_out.filter((f) => !pick.leftOut.includes(f)));
      pick.folders.push(...listing.empty_folders);
      await addProject(listing.name, pick);
    } catch (e) {
      setPickError(e instanceof Error ? e.message : String(e));
    }
  }, [addProject]);

  const clear = useCallback(
    () => setItems((prev) => prev.filter((i) => i.status === "queued" || i.status === "uploading")),
    [],
  );

  const busy = items.some((i) => i.status === "queued" || i.status === "uploading");
  return {
    items,
    busy,
    version,
    pick,
    pickFolder,
    pickZip,
    addFiles,
    addDropped,
    addPickedFolder,
    clear,
    pickError,
    fallbackInput,
    folderInput,
    zipInput,
  };
}

/** The hidden <input type="file"> used outside the desktop shell. */
export function FallbackInput({
  inputRef,
  onFiles,
  accept = ACCEPT,
}: {
  inputRef: React.MutableRefObject<HTMLInputElement | null>;
  onFiles: (files: File[]) => void;
  accept?: string;
}) {
  return (
    <input
      ref={inputRef}
      type="file"
      multiple
      hidden
      accept={accept}
      onChange={(e) => {
        onFiles(Array.from(e.target.files ?? []));
        e.target.value = "";
      }}
    />
  );
}

/** The hidden folder picker used outside the desktop shell. */
export function FallbackFolderInput({
  inputRef,
  onFiles,
}: {
  inputRef: React.MutableRefObject<HTMLInputElement | null>;
  onFiles: (files: File[]) => void;
}) {
  // `webkitdirectory` isn't in React's input types, though every browser
  // supports it.
  const folder = { webkitdirectory: "", directory: "" } as Record<string, string>;
  return (
    <input
      ref={inputRef}
      type="file"
      hidden
      multiple
      {...folder}
      onChange={(e) => {
        onFiles(Array.from(e.target.files ?? []));
        e.target.value = "";
      }}
    />
  );
}

/**
 * Files dragged over any part of the window light up a full-window target.
 * Drag events fire for every child crossed, so depth is counted. A dropped
 * folder becomes a project.
 */
export function DropOverlay({
  onDrop: onDropped,
  enabled,
}: {
  onDrop: (folders: FileSystemDirectoryEntry[], files: File[]) => void;
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
      if (!enabled || !e.dataTransfer) return;
      // Entries must be taken during the event; the list is emptied after.
      const folders: FileSystemDirectoryEntry[] = [];
      const files: File[] = [];
      for (const item of Array.from(e.dataTransfer.items)) {
        if (item.kind !== "file") continue;
        const entry = item.webkitGetAsEntry?.();
        if (entry?.isDirectory) {
          folders.push(entry as FileSystemDirectoryEntry);
        } else {
          const file = item.getAsFile();
          if (file) files.push(file);
        }
      }
      onDropped(folders, files);
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
  }, [enabled, onDropped]);

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
        {enabled && <p className="drop-sub">Folders and .zip files become projects.</p>}
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
  stored: { label: "Kept", icon: Archive, tone: "success" },
  skipped: { label: "Skipped", icon: CircleSlash, tone: "neutral" },
  rejected: { label: "Not added", icon: CircleSlash, tone: "danger" },
};

/** Bottom-right card that follows the current batch of uploads. */
export function UploadTray({
  items,
  onOpen,
  onOpenProject,
  onClear,
}: {
  items: UploadItem[];
  onOpen: (artifactId: string) => void;
  onOpenProject: (projectId: string) => void;
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
              : `${plural(
                  items.filter((i) => i.status === "accepted" || i.status === "stored").length,
                  "file",
                )} added`}
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
                    {item.project && <span className="dot-sep">{item.project}</span>}
                    {item.detail && <span className="dot-sep tray-detail">{item.detail}</span>}
                  </span>
                </span>
                {item.artifactId ? (
                  <IconButton
                    icon={ArrowUpRight}
                    label={`Open ${item.name} in the Library`}
                    size="sm"
                    tip="left"
                    onClick={() => onOpen(item.artifactId!)}
                  />
                ) : (
                  item.projectId &&
                  item.status === "accepted" && (
                    <IconButton
                      icon={ArrowUpRight}
                      label={`Open the project ${item.project ?? ""}`}
                      size="sm"
                      tip="left"
                      onClick={() => onOpenProject(item.projectId!)}
                    />
                  )
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
