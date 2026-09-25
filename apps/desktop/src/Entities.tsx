import { useCallback, useEffect, useState } from "react";
import { Check, ChevronRight, CircleCheck, History, Info, Unlink } from "lucide-react";
import {
  dismissMergeSuggestion,
  getEntity,
  listMergeSuggestions,
  mergeEntities,
  type EntityDetail,
  type EntityRef,
  type MergeSuggestion,
} from "./api";
import { plural } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  KindTag,
  Meter,
  PageHeader,
  Skeleton,
  When,
  errorText,
} from "./ui";

const METHOD_LABELS: Record<string, string> = {
  "rule:name-similarity": "Similar names",
  "embedding:cosine": "Similar meaning",
};

/** Aliases + merge history for one side of a suggested pair. */
function EntityFacts({ id }: { id: string }) {
  const [detail, setDetail] = useState<EntityDetail | null>(null);

  useEffect(() => {
    let cancelled = false;
    getEntity(id)
      .then((d) => {
        if (!cancelled) setDetail(d);
      })
      .catch(() => {
        /* detail is supplementary; the pair is still actionable without it */
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  if (!detail) return <Skeleton rows={1} />;
  return (
    <div className="entity-facts">
      <p className="hint">Known since {new Date(detail.created_at).toLocaleDateString()}</p>
      {detail.description && <p className="entity-desc">{detail.description}</p>}
      {detail.aliases.length > 0 && (
        <div className="aliases" aria-label="Also known as">
          {detail.aliases.map((a) => (
            <Badge key={a}>{a}</Badge>
          ))}
        </div>
      )}
      {detail.audit.length > 0 && (
        // Prior merge decisions are context for this one, which is not
        // casually reversible — so surface them rather than just fetch them.
        <details className="history">
          <summary>
            <History aria-hidden /> Merge history{" "}
            <span className="count num">{detail.audit.length}</span>
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

function Detail({ suggestion, onDone }: { suggestion: MergeSuggestion; onDone: () => void }) {
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async (which: string, fn: () => Promise<void>) => {
    setBusy(which);
    setError(null);
    try {
      await fn();
      onDone();
    } catch (e) {
      setError(errorText(e));
      setBusy(null);
    }
  };

  const merge = (winner: EntityRef, loser: EntityRef) =>
    run(winner.id, () => mergeEntities(winner.id, loser.id, note.trim() || undefined));

  return (
    <div className="item-body">
      <div className="versus">
        {[suggestion.a, suggestion.b].map((side, i) => {
          const other = i === 0 ? suggestion.b : suggestion.a;
          return (
            <section
              className="versus-side"
              key={side.id}
              aria-label={`Entity ${i === 0 ? "A" : "B"}`}
            >
              <div className="versus-head">
                <span className="versus-letter" aria-hidden>
                  {i === 0 ? "A" : "B"}
                </span>
                <KindTag kind={side.kind} />
              </div>
              <p className="versus-statement">{side.name}</p>
              <EntityFacts id={side.id} />
              <Button
                variant="secondary"
                size="sm"
                icon={Check}
                loading={busy === side.id}
                disabled={busy !== null}
                onClick={() => merge(side, other)}
                title={`Keep “${side.name}” and fold “${other.name}” into it`}
              >
                Keep “{side.name}”
              </Button>
            </section>
          );
        })}
      </div>

      <Callout tone="neutral" icon={Info}>
        Merging keeps one entity and folds the other into it as an alias: its statements,
        connections and sources move across, and the old name keeps resolving to the one you kept.
      </Callout>

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
            variant="ghost"
            size="sm"
            icon={Unlink}
            loading={busy === "dismiss"}
            disabled={busy !== null}
            onClick={() =>
              run("dismiss", () =>
                dismissMergeSuggestion(suggestion.a.id, suggestion.b.id, note.trim() || undefined),
              )
            }
          >
            Not duplicates
          </Button>
        </div>
      </div>

      {error && <Callout>{error}</Callout>}
    </div>
  );
}

export default function Entities() {
  const [items, setItems] = useState<MergeSuggestion[] | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    listMergeSuggestions()
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
        title="Entities"
        description="Pairs that might be the same person, place or thing under different names. Clear matches merge on their own; these are the close calls."
        eyebrow={items && items.length > 0 ? plural(items.length, "suggestion") : undefined}
      />
      {error && <Callout title="Couldn't load suggestions">{error}</Callout>}
      {items === null && !error ? (
        <Skeleton rows={3} variant="card" />
      ) : items && items.length === 0 && !error ? (
        <EmptyState icon={CircleCheck} tone="success" title="No duplicates to check">
          Your knowledge graph looks deduplicated. New suggestions appear here as you add more.
        </EmptyState>
      ) : (
        <ul className="stack">
          {(items ?? []).map((s, i) => {
            const key = `${s.a.id}:${s.b.id}`;
            const open = expanded === key;
            return (
              <li
                key={key}
                className={open ? "item open" : "item"}
                style={{ animationDelay: `${Math.min(i, 8) * 30}ms` }}
              >
                <button
                  type="button"
                  className="item-row"
                  aria-expanded={open}
                  onClick={() => setExpanded(open ? null : key)}
                >
                  <div className="item-main">
                    <div className="pair-names">
                      <span className="item-title">{s.a.name}</span>
                      <span className="pair-sep" aria-label="and">
                        ≈
                      </span>
                      <span className="item-title">{s.b.name}</span>
                    </div>
                    <div className="item-sub">
                      <KindTag kind={s.a.kind} />
                      <span>{METHOD_LABELS[s.method] ?? s.method}</span>
                    </div>
                  </div>
                  <div className="item-aside">
                    <Meter value={s.score} label="Similarity" />
                    <ChevronRight className="chevron" aria-hidden />
                  </div>
                </button>
                {open && (
                  <Detail
                    suggestion={s}
                    onDone={() => {
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
