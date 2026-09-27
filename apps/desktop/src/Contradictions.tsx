import { useCallback, useEffect, useRef, useState } from "react";
import {
  Check,
  CircleCheck,
  CircleSlash,
  GitCompareArrows,
  History,
  MessageSquarePlus,
  Quote,
  X,
} from "lucide-react";
import {
  agreeExplainedAway,
  annotateContradiction,
  confirmExplainedAway,
  getContradiction,
  listContradictions,
  listExplainedAway,
  resolveContradiction,
  type ContradictionDetail,
  type ContradictionSummary,
  type ExplainedAway,
  type Provenance,
  type Resolution,
} from "./api";
import { useListKeys } from "./hooks/useListKeys";
import Why from "./Why";
import { kindLabel } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Meter,
  Segmented,
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

  // The id a response belongs to: a slower answer for a pair the user has
  // already moved away from must not replace the current one.
  const current = useRef(id);
  current.current = id;
  const reload = useCallback(() => {
    getContradiction(id)
      .then((d) => {
        if (current.current === id) setDetail(d);
      })
      .catch((e) => {
        if (current.current === id) setError(errorText(e));
      });
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

      <Why query={{ conclusion_id: id }} limit={1} />

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

type Mode = "open" | "explained";

const PAGE = 100;

/** A flagged pair Gather decided isn't a contradiction, for a spot-check. */
function ExplainedDetail({
  item,
  onDecided,
}: {
  item: ExplainedAway;
  onDecided: (label: string, contradictionId?: string) => void;
}) {
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState<"confirm" | "agree" | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setNote("");
    setError(null);
  }, [item.certificate_id]);

  const decide = async (verdict: "confirm" | "agree") => {
    setBusy(verdict);
    setError(null);
    try {
      if (verdict === "confirm") {
        const r = await confirmExplainedAway(item.certificate_id, note.trim() || undefined);
        onDecided("Marked as a real conflict", r.contradiction_id);
      } else {
        await agreeExplainedAway(item.certificate_id, note.trim() || undefined);
        onDecided("Agreed: not a conflict");
      }
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="inspector" key={item.certificate_id}>
      <div className="inspector-kicker">
        <Badge tone="neutral" icon={CircleSlash}>
          Not reported
        </Badge>
        <span className="hint">
          {methodLabel(item.detection_method)} · decided <When iso={item.decided_at} />
        </span>
      </div>
      <h2 className="inspector-title">Is this really not a conflict?</h2>
      <p className="explain">
        These statements looked contradictory, but Gather found a reason both can be true. If that
        reason is wrong (for example a misread date), mark it as a real conflict.
      </p>

      <Callout tone="info" title="Why it wasn't reported">
        <ul className="why-reasons">
          {item.reasons.map((r) => (
            <li key={r.code} title={r.code}>
              {r.text || r.code}
            </li>
          ))}
        </ul>
      </Callout>

      <div className="versus">
        {(["unit_a", "unit_b"] as const).map((side) => {
          const letter = side === "unit_a" ? "A" : "B";
          const claim = item[side];
          return (
            <section className="versus-side" key={side} aria-label={`Statement ${letter}`}>
              <div className="versus-head">
                <span className="versus-letter" aria-hidden>
                  {letter}
                </span>
                {claim.valid_from && (
                  <span className="hint">
                    since {new Date(claim.valid_from).toLocaleDateString()}
                  </span>
                )}
                {claim.status === "superseded" && <Badge tone="neutral">kept as history</Badge>}
              </div>
              <p className="versus-statement">{claim.statement}</p>
            </section>
          );
        })}
      </div>

      <Why certificateId={item.certificate_id} limit={1} />

      {error && <Callout>{error}</Callout>}

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
        <span className="spacer" />
        <Button
          variant="secondary"
          icon={CircleCheck}
          loading={busy === "agree"}
          disabled={busy !== null}
          onClick={() => decide("agree")}
        >
          Explanation is right
        </Button>
        <Button
          variant="primary"
          icon={GitCompareArrows}
          loading={busy === "confirm"}
          disabled={busy !== null}
          onClick={() => decide("confirm")}
        >
          It's a real conflict
        </Button>
      </div>
    </div>
  );
}

function ExplainedList({
  items,
  selected,
  onSelect,
}: {
  items: ExplainedAway[];
  selected: string | null;
  onSelect: (id: string) => void;
}) {
  const listRef = useListKeys(items, selected, (c) => c.certificate_id, onSelect);
  return (
    <ul className="rows" ref={listRef}>
      {items.map((c) => (
        <li key={c.certificate_id}>
          <button
            type="button"
            className="row"
            aria-current={c.certificate_id === selected ? "true" : undefined}
            onClick={() => onSelect(c.certificate_id)}
          >
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
              <span className="row-meta clamp1">
                {c.reasons[0]?.text ?? methodLabel(c.detection_method)}
              </span>
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}

export default function Contradictions() {
  const [items, setItems] = useState<ContradictionSummary[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [mode, setMode] = useState<Mode>("open");
  const [explained, setExplained] = useState<{
    items: ExplainedAway[];
    total: number;
  } | null>(null);
  const [picked, setPicked] = useState<string | null>(null);
  const [explainedError, setExplainedError] = useState<string | null>(null);
  // How many explained-away pairs to load; "Show more" raises it.
  const [shown, setShown] = useState(PAGE);

  const refresh = useCallback(
    () =>
      Promise.all([
        listContradictions("open")
          .then((list) => {
            setItems(list);
            setError(null);
          })
          .catch((e) => setError(errorText(e))),
        listExplainedAway(shown)
          .then((page) => {
            setExplained(page);
            setExplainedError(null);
          })
          .catch((e) => setExplainedError(errorText(e))),
      ]),
    [shown],
  );

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

  useEffect(() => {
    const list = explained?.items;
    if (!list || list.length === 0) return;
    if (!picked || !list.some((c) => c.certificate_id === picked))
      setPicked(list[0].certificate_id);
  }, [explained, picked]);

  const listRef = useListKeys(items ?? [], selected, (c) => c.id, setSelected);
  const current = explained?.items.find((c) => c.certificate_id === picked);

  return (
    <>
      <Toolbar
        title="Contradictions"
        icon={GitCompareArrows}
        count={items && items.length > 0 ? items.length : undefined}
      >
        <Segmented
          label="Show"
          options={[
            { value: "open" as Mode, label: "Open", count: items?.length },
            {
              value: "explained" as Mode,
              label: "Explained away",
              count: explained?.total,
            },
          ]}
          value={mode}
          onChange={setMode}
        />
      </Toolbar>
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
      {mode === "explained" && explainedError && (
        <div className="view-callout">
          <Callout title="Couldn't load explained-away pairs">{explainedError}</Callout>
        </div>
      )}
      {mode === "explained" ? (
        explained !== null && explained.items.length === 0 && !explainedError ? (
          <EmptyState icon={CircleCheck} tone="success" title="Nothing to check">
            When Gather decides two statements that look contradictory can both be true, the pair
            shows up here with the reason, so you can check it.
          </EmptyState>
        ) : (
          <SplitView
            listLabel="Explained away"
            listHeader={
              <p className="hint list-note">
                Looked contradictory, but not reported. Check the reason holds.
              </p>
            }
            list={
              explained === null ? (
                explainedError ? null : (
                  <Skeleton rows={5} />
                )
              ) : (
                <>
                  <ExplainedList items={explained.items} selected={picked} onSelect={setPicked} />
                  {explained.items.length < explained.total && (
                    <div className="list-more">
                      <Button variant="ghost" onClick={() => setShown((n) => n + PAGE)}>
                        Show more · {explained.total - explained.items.length} left
                      </Button>
                    </div>
                  )}
                </>
              )
            }
            detail={
              current ? (
                <ExplainedDetail
                  item={current}
                  onDecided={(label, contradictionId) => {
                    setDone(label);
                    setPicked(null);
                    // Show the new contradiction once the list has it, so the
                    // selection isn't reset to the first row meanwhile.
                    refresh().then(() => {
                      if (contradictionId) {
                        setSelected(contradictionId);
                        setMode("open");
                      }
                    });
                  }}
                />
              ) : null
            }
          />
        )
      ) : items !== null && items.length === 0 && !error ? (
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
