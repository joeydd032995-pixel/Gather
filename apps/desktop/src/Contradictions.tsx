import { useCallback, useEffect, useState } from "react";
import {
  Check,
  CircleCheck,
  GitCompareArrows,
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
import { useListKeys } from "./hooks/useListKeys";
import { kindLabel } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Meter,
  Skeleton,
  SplitView,
  Toolbar,
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
  useEffect(() => {
    setDetail(null);
    setNote("");
    reload();
  }, [reload]);

  const act = async (resolution: Resolution, label: string) => {
    setBusy(resolution);
    setError(null);
    try {
      await resolveContradiction(id, resolution, note.trim() || undefined);
      onResolved(label);
    } catch (e) {
      setError(errorText(e));
    } finally {
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
      <div className="inspector">{error ? <Callout>{error}</Callout> : <Skeleton rows={4} />}</div>
    );
  }

  return (
    <div className="inspector" key={id}>
      <div className="inspector-kicker">
        <Badge tone={detail.score >= 0.75 ? "danger" : "warning"} icon={GitCompareArrows}>
          {methodLabel(detail.detection_method)}
        </Badge>
        <Meter
          value={detail.score}
          label="Conflict strength"
          tone={detail.score >= 0.75 ? "danger" : "warning"}
          width={64}
        />
        <span className="hint">
          found <When iso={detail.detected_at} />
        </span>
      </div>
      <h2 className="inspector-title">Which one holds?</h2>
      {detail.explanation && <p className="explain">{detail.explanation}</p>}

      <div className="versus">
        {(["unit_a", "unit_b"] as const).map((side) => {
          const letter = side === "unit_a" ? "A" : "B";
          const resolution: Resolution = side === "unit_a" ? "resolved_a" : "resolved_b";
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
                icon={Check}
                className="versus-keep"
                loading={busy === resolution}
                disabled={busy !== null}
                onClick={() => act(resolution, `Kept statement ${letter}`)}
              >
                Keep {letter}
              </Button>
            </section>
          );
        })}
      </div>

      {error && <Callout>{error}</Callout>}

      {detail.audit.length > 0 && (
        <details className="history">
          <summary>
            <History aria-hidden /> History <span className="num">{detail.audit.length}</span>
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

      <div className="inspector-actions">
        <input
          className="input note-input"
          type="text"
          aria-label="Note (optional)"
          placeholder="Add a note (optional)…"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          disabled={busy !== null}
        />
        <Button
          variant="ghost"
          icon={MessageSquarePlus}
          loading={busy === "annotate"}
          disabled={busy !== null || !note.trim()}
          onClick={annotate}
        >
          Add note
        </Button>
        <span className="spacer" />
        <Button
          variant="secondary"
          icon={CircleCheck}
          loading={busy === "both_valid"}
          disabled={busy !== null}
          onClick={() => act("both_valid", "Marked both as valid")}
        >
          Both valid
        </Button>
        <Button
          variant="ghost"
          icon={X}
          loading={busy === "dismissed"}
          disabled={busy !== null}
          onClick={() => act("dismissed", "Dismissed")}
        >
          Dismiss
        </Button>
      </div>
    </div>
  );
}

export default function Contradictions() {
  const [items, setItems] = useState<ContradictionSummary[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
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

  // Keep a valid selection as items come and go.
  useEffect(() => {
    if (!items || items.length === 0) return;
    if (!selected || !items.some((c) => c.id === selected)) setSelected(items[0].id);
  }, [items, selected]);

  const listRef = useListKeys(items ?? [], selected, (c) => c.id, setSelected);

  return (
    <>
      <Toolbar
        title="Contradictions"
        icon={GitCompareArrows}
        count={items && items.length > 0 ? items.length : undefined}
      />
      {done && (
        <div className="visually-hidden" role="status">
          {done}
        </div>
      )}
      {error && (
        <div className="view-callout">
          <Callout title="Couldn't load contradictions">{error}</Callout>
        </div>
      )}
      {items !== null && items.length === 0 && !error ? (
        <EmptyState icon={CircleCheck} tone="success" title="Everything agrees">
          No open contradictions. Gather keeps scanning in the background as you add more.
        </EmptyState>
      ) : (
        <SplitView
          listLabel="Open contradictions"
          listHeader={
            <p className="hint list-note">Strongest first. Pick the statement that holds.</p>
          }
          list={
            items === null ? (
              <Skeleton rows={5} />
            ) : (
              <ul className="rows" ref={listRef}>
                {items.map((c) => (
                  <li key={c.id}>
                    <button
                      type="button"
                      className="row"
                      aria-current={c.id === selected ? "true" : undefined}
                      onClick={() => setSelected(c.id)}
                    >
                      <span
                        className={`row-strength ${c.score >= 0.75 ? "hi" : "mid"}`}
                        style={{ height: `${Math.max(30, c.score * 100)}%` }}
                        aria-hidden
                      />
                      <span className="row-main">
                        <span className="conflict-line">
                          <span className="pair-letter" aria-hidden>
                            A
                          </span>
                          <span className="clamp1">{c.unit_a.statement}</span>
                        </span>
                        <span className="conflict-line">
                          <span className="pair-letter" aria-hidden>
                            B
                          </span>
                          <span className="clamp1">{c.unit_b.statement}</span>
                        </span>
                        <span className="row-meta">
                          {methodLabel(c.detection_method)}
                          <span className="dot-sep num">{c.score.toFixed(2)}</span>
                        </span>
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )
          }
          detail={
            selected ? (
              <Detail
                id={selected}
                onResolved={(label) => {
                  setDone(label);
                  setSelected(null);
                  refresh();
                }}
              />
            ) : null
          }
        />
      )}
    </>
  );
}
