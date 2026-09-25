import { useRef, useState } from "react";
import {
  ArrowRight,
  CircleCheck,
  CircleSlash,
  Copy,
  FilePlus,
  GitCompareArrows,
  ScanText,
  ShieldCheck,
  Upload as UploadIcon,
  Waypoints,
} from "lucide-react";
import type { FileResult } from "./api";
import { kindIcon, kindLabel, plural } from "./kinds";
import { Badge, Button, Callout, PageHeader } from "./ui";

const ACCEPTED = ["PDF", "Markdown", "Text", "PNG", "JPEG", "WebP", "TIFF", "HEIC"];

const STEPS = [
  {
    icon: ScanText,
    title: "Read",
    body: "Text, pages and photos are read locally, including text in screenshots.",
  },
  {
    icon: Waypoints,
    title: "Connect",
    body: "Facts, decisions and people are linked into a graph, with the source of each.",
  },
  {
    icon: GitCompareArrows,
    title: "Reconcile",
    body: "Duplicates merge on their own, and disagreements are flagged for you.",
  },
];

interface UploadProps {
  ready: boolean;
  busy: boolean;
  progress: { done: number; total: number } | null;
  results: FileResult[];
  error: string | null;
  onPick: () => void;
  onDropFiles: (files: File[]) => void;
  onOpenFile: (id: string) => void;
  onClear: () => void;
}

function StatusBadge({ result }: { result: FileResult }) {
  if (result.status === "accepted") {
    return (
      <Badge tone="success" icon={CircleCheck}>
        Added
      </Badge>
    );
  }
  if (result.status === "deduplicated") {
    return (
      <Badge tone="warning" icon={Copy} title="Gather already had this exact file">
        Already added
      </Badge>
    );
  }
  return (
    <Badge tone="danger" icon={CircleSlash}>
      Not added
    </Badge>
  );
}

/** Add documents and photos: drop them in, or pick them. */
export default function Upload({
  ready,
  busy,
  progress,
  results,
  error,
  onPick,
  onDropFiles,
  onOpenFile,
  onClear,
}: UploadProps) {
  // Nested children fire dragleave as the pointer crosses them; count depth.
  const depth = useRef(0);
  const [dragging, setDragging] = useState(false);

  const added = results.filter((r) => r.status !== "rejected").length;
  const pct = progress ? ((progress.done + 0.5) / progress.total) * 100 : 0;

  return (
    <>
      <PageHeader
        title="Add to your library"
        description="Drop in PDFs, notes, screenshots and photos. Gather reads each one on this computer, finds the facts, decisions and people in it, and links them to what you already have."
      />

      <section
        className={`dropzone${dragging ? " dragging" : ""}${!ready ? " disabled" : ""}`}
        aria-label="Drop files here to add them"
        onDragEnter={(e) => {
          e.preventDefault();
          depth.current += 1;
          setDragging(true);
        }}
        onDragOver={(e) => e.preventDefault()}
        onDragLeave={() => {
          depth.current = Math.max(0, depth.current - 1);
          if (depth.current === 0) setDragging(false);
        }}
        onDrop={(e) => {
          e.preventDefault();
          depth.current = 0;
          setDragging(false);
          onDropFiles(Array.from(e.dataTransfer.files));
        }}
      >
        <div className="dropzone-art" aria-hidden>
          <span className="dropzone-sheet s1" />
          <span className="dropzone-sheet s2" />
          <span className="dropzone-sheet s3">
            <UploadIcon />
          </span>
        </div>
        <h2 className="dropzone-title">
          {dragging ? "Release to add these files" : "Drag files here"}
        </h2>
        <p className="dropzone-sub">
          or{" "}
          <Button
            variant="primary"
            icon={FilePlus}
            onClick={onPick}
            disabled={busy || !ready}
            className="dropzone-btn"
          >
            Choose files…
          </Button>
        </p>
        <ul className="dropzone-types" aria-label="Accepted file types">
          {ACCEPTED.map((t) => (
            <li key={t}>{t}</li>
          ))}
        </ul>

        {progress && (
          <div className="upload-progress" role="status" aria-live="polite">
            <div className="upload-progress-label">
              <span>
                Adding file {progress.done + 1} of {progress.total}…
              </span>
              <span className="num">{Math.round(pct)}%</span>
            </div>
            <div className="progress-track">
              <div className="progress-fill" style={{ width: `${pct}%` }} />
            </div>
          </div>
        )}
      </section>

      <p className="privacy-note">
        <ShieldCheck aria-hidden />
        Files never leave this computer. Gather has no account, no cloud and no telemetry.
      </p>

      {!ready && (
        <Callout tone="warning" title="Waiting for the local daemon">
          Adding files needs Gather's background service. It usually takes a few seconds to start.
        </Callout>
      )}
      {error && <Callout title="Couldn't open the file picker">{error}</Callout>}

      {results.length === 0 && (
        <ol className="steps" aria-label="What happens next">
          {STEPS.map(({ icon: Icon, title, body }, i) => (
            <li key={title} className="step">
              <span className="step-icon" aria-hidden>
                <Icon />
              </span>
              <span className="step-num num" aria-hidden>
                0{i + 1}
              </span>
              <h3 className="step-title">{title}</h3>
              <p className="step-body">{body}</p>
            </li>
          ))}
        </ol>
      )}

      {results.length > 0 && (
        <section className="upload-results" aria-labelledby="recent-heading">
          <div className="section-label">
            <span id="recent-heading">This session</span>
            <span className="count">{plural(added, "file")} added</span>
            <Button variant="ghost" size="sm" onClick={onClear} style={{ marginLeft: "auto" }}>
              Clear
            </Button>
          </div>
          <ul className="result-list">
            {results.map((r, i) => {
              const Icon = kindIcon(r.kind);
              const open = r.artifact_id ? () => onOpenFile(r.artifact_id!) : undefined;
              return (
                <li key={`${r.artifact_id ?? r.filename}-${i}`} className="result-row">
                  <span className="file-icon" aria-hidden>
                    <Icon />
                  </span>
                  <div className="result-main">
                    <span className="result-name">{r.filename}</span>
                    <span className="result-meta">
                      {r.kind ? kindLabel(r.kind) : "—"}
                      {r.segments > 0 && (
                        <span className="dot-sep">{plural(r.segments, "section")}</span>
                      )}
                      {r.detail && <span className="dot-sep result-detail">{r.detail}</span>}
                    </span>
                  </div>
                  <StatusBadge result={r} />
                  {open && (
                    <Button variant="ghost" size="sm" icon={ArrowRight} onClick={open}>
                      <span className="visually-hidden">Open {r.filename} in the </span>Library
                    </Button>
                  )}
                </li>
              );
            })}
          </ul>
          <p className="hint upload-foot">
            Gather reads each file in the background; what it finds shows up in the Library within a
            minute or two.
          </p>
        </section>
      )}
    </>
  );
}
