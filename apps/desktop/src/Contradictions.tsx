import { useCallback, useEffect, useState } from "react";
import {
  Check,
  ChevronRight,
  CircleCheck,
  History,
  MessageSquarePlus,
  Quote,
  X,
} from "lucide-react";
import {
  annotateContradiction,
  getContradiction,
  listContradictions,
  resolveContradiction,
  type ContradictionDetail,
  type ContradictionSummary,
  type Provenance,
  type Resolution,
} from "./api";
import { kindLabel, plural } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Meter,
  PageHeader,
  Skeleton,
  When,
  errorText,
} from "./ui";

const METHOD_LABELS: Record<string, string> = {
  "numeric-mismatch": "Different numbers",
  numeric: "Different numbers",
  negation: "One negates the other",
  "exclusive-assignment": "Only one can be true",
  exclusive: "Only one can be true",
  antonym: "Opposite meanings",
};

function methodLabel(method: string): string {
  const bare = method.replace(/^rule:/, "");
  return METHOD_LABELS[bare] ?? bare.replace(/[-_:]/g, " ");
}

function ProvenanceList({ items }: { items: Provenance[] }) {
  if (items.length === 0) return <p className="hint">No source recorded.</p>;
  return (
    <ul className="sources">
      {items.map((p, i) => (
        <li key={i} className="source">
          <div className="source-meta">
            <Badge tone="info">{p.source_platform}</Badge>
            <span className="source-name">{p.original_filename ?? kindLabel(p.artifact_kind)}</span>
            <When iso={p.ingested_at} className="source-time" />
          </div>
          {p.quote && (
            <blockquote className="source-quote">
              <Quote aria-hidden />
              {p.quote}
            </blockquote>
          )}
        </li>
      ))}
    </ul>
  );
}

function Detail({ id, onResolved }: { id: string; onResolved: (label: string) => void }) {
  const [detail, setDetail] = useState<ContradictionDetail | null>(null);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState<Resolution | "annotate" | null>(null);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    getContradiction(id)
      .then(setDetail)
      .catch((e) => setError(errorText(e)));
  }, [id]);
  useEffect(reload, [reload]);

  const act = async (resolution: Resolution, label: string) => {
    setBusy(resolution);
    setError(null);
    try {
      await resolveContradiction(id, resolution, note.trim() || undefined);
      onResolved(label);
    } catch (e) {
      setError(errorText(e));
      setBusy(null);
    }
  };

  const annotate = async () => {
    if (!note.trim()) return;
    setBusy("annotate");
    setError(null);
    try {
      await annotateContradiction(id, note.trim());
      setNote("");
      reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  if (!detail) {
    return (
      <div className="item-body">{error ? <Callout>{error}</Callout> : <Skeleton rows={2} />}</div>
    );
  }

  return (
    <div className="item-body">
      {detail.explanation && <p className="explanation">{detail.explanation}</p>}

      <div className="versus">
        {(["unit_a", "unit_b"] as const).map((side) => {
          const letter = side === "unit_a" ? "A" : "B";
          return (
            <section className="versus-side" key={side} aria-label={`Statement ${letter}`}>
              <div className="versus-head">
                <span className="versus-letter" aria-hidden>
                  {letter}
                </span>
                {detail[side].valid_from && (
                  <span className="hint">
                    since {new Date(detail[side].valid_from!).toLocaleDateString()}
                  </span>
                )}
              </div>
              <p className="versus-statement">{detail[side].statement}</p>
              <ProvenanceList items={detail[side].provenance} />
              <Button
                variant="secondary"
                size="sm"
                icon={Check}
                loading={busy === (side === "unit_a" ? "resolved_a" : "resolved_b")}
                disabled={busy !== null}
                onClick={() =>
                  act(side === "unit_a" ? "resolved_a" : "resolved_b", `Kept statement ${letter}`)
                }
              >
                Keep {letter}
              </Button>
            </section>
          );
        })}
      </div>

      <div className="resolve-bar">
        <input
          className="input"
          type="text"
          aria-label="Note (optional)"
          placeholder="Add a note (optional)…"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          disabled={busy !== null}
        />
        <div className="item-actions">
          <Button
            variant="secondary"
            size="sm"
            icon={CircleCheck}
            loading={busy === "both_valid"}
            disabled={busy !== null}
            onClick={() => act("both_valid", "Marked both as valid")}
          >
            Both valid
          </Button>
          <Button
            variant="ghost"
            size="sm"
            icon={X}
            loading={busy === "dismissed"}
            disabled={busy !== null}
            onClick={() => act("dismissed", "Dismissed")}
          >
            Dismiss
          </Button>
          <Button
            variant="ghost"
            size="sm"
            icon={MessageSquarePlus}
            loading={busy === "annotate"}
            disabled={busy !== null || !note.trim()}
            onClick={annotate}
          >
            Add note
          </Button>
        </div>
      </div>

      {error && <Callout>{error}</Callout>}

      {detail.audit.length > 0 && (
        <details className="history">
          <summary>
            <History aria-hidden /> History <span className="count num">{detail.audit.length}</span>
          </summary>
          <ol className="timeline">
            {detail.audit.map((a, i) => (
              <li key={i}>
                <When iso={a.created_at} className="timeline-time" />
                <span>
                  <strong>{a.actor}</strong> {a.action}
                  {a.note ? ` — ${a.note}` : ""}
                </span>
              </li>
            ))}
          </ol>
        </details>
      )}
    </div>
  );
}

export default function Contradictions() {
  const [items, setItems] = useState<ContradictionSummary[] | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);

  const refresh = useCallback(() => {
    listContradictions("open")
      .then((list) => {
        setItems(list);
        setError(null);
      })
      .catch((e) => setError(errorText(e)));
  }, []);

  useEffect(() => {
    refresh();
    const timer = setInterval(refresh, 15000);
    return () => clearInterval(timer);
  }, [refresh]);

  return (
    <section>
      <PageHeader
        title="Contradictions"
        description="Places where your sources disagree with each other, strongest first. Pick the statement that holds, or mark both as true."
        eyebrow={items && items.length > 0 ? `${plural(items.length, "open conflict")}` : undefined}
      />
      {error && <Callout title="Couldn't load contradictions">{error}</Callout>}
      {done && (
        <div className="sr-status visually-hidden" role="status">
          {done}
        </div>
      )}
      {items === null && !error ? (
        <Skeleton rows={3} variant="card" />
      ) : items && items.length === 0 && !error ? (
        <EmptyState icon={CircleCheck} tone="success" title="Everything agrees">
          No open contradictions. Gather keeps scanning in the background as you add more.
        </EmptyState>
      ) : (
        <ul className="stack">
          {(items ?? []).map((c, i) => {
            const open = expanded === c.id;
            return (
              <li
                key={c.id}
                className={open ? "item open" : "item"}
                style={{ animationDelay: `${Math.min(i, 8) * 30}ms` }}
              >
                <button
                  type="button"
                  className="item-row"
                  aria-expanded={open}
                  onClick={() => setExpanded(open ? null : c.id)}
                >
                  <div className="item-main">
                    <div className="conflict-pair">
                      <span className="conflict-line">
                        <span className="pair-letter" aria-hidden>
                          A
                        </span>
                        {c.unit_a.statement}
                      </span>
                      <span className="conflict-line">
                        <span className="pair-letter" aria-hidden>
                          B
                        </span>
                        {c.unit_b.statement}
                      </span>
                    </div>
                    <div className="item-sub">
                      <span>{methodLabel(c.detection_method)}</span>
                      <span>
                        found <When iso={c.detected_at} />
                      </span>
                    </div>
                  </div>
                  <div className="item-aside">
                    <Meter
                      value={c.score}
                      label="Conflict strength"
                      tone={c.score >= 0.75 ? "danger" : "warning"}
                    />
                    <ChevronRight className="chevron" aria-hidden />
                  </div>
                </button>
                {open && (
                  <Detail
                    id={c.id}
                    onResolved={(label) => {
                      setDone(label);
                      setExpanded(null);
                      refresh();
                    }}
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
