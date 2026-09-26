import { useCallback, useEffect, useState } from "react";
import { Check, CircleCheck, Combine, History, Info, Unlink } from "lucide-react";
import {
  dismissMergeSuggestion,
  getEntity,
  listMergeSuggestions,
  mergeEntities,
  type EntityDetail,
  type EntityRef,
  type MergeSuggestion,
} from "./api";
import { useListKeys } from "./hooks/useListKeys";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  KindTag,
  Meter,
  Skeleton,
  SplitView,
  Toolbar,
  When,
  errorText,
} from "./ui";

const METHOD_LABELS: Record<string, string> = {
  "rule:name-similarity": "Similar names",
  "embedding:cosine": "Similar meaning",
};

const keyOf = (s: MergeSuggestion) => `${s.a.id}:${s.b.id}`;

function initials(name: string): string {
  return (
    name
      .split(/\s+/)
      .filter(Boolean)
      .slice(0, 2)
      .map((w) => w[0]!.toUpperCase())
      .join("") || "?"
  );
}

/** Aliases + merge history for one side of a suggested pair. */
function EntityFacts({ id }: { id: string }) {
  const [detail, setDetail] = useState<EntityDetail | null>(null);

  useEffect(() => {
    let cancelled = false;
    setDetail(null);
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
            <History aria-hidden /> Merge history <span className="num">{detail.audit.length}</span>
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
    } finally {
      setBusy(null);
    }
  };

  const merge = (winner: EntityRef, loser: EntityRef) =>
    run(winner.id, () => mergeEntities(winner.id, loser.id, note.trim() || undefined));

  return (
    <div className="inspector" key={keyOf(suggestion)}>
      <div className="inspector-kicker">
        <Badge tone="info" icon={Combine}>
          {METHOD_LABELS[suggestion.method] ?? suggestion.method}
        </Badge>
        <Meter value={suggestion.score} label="Similarity" width={64} />
      </div>
      <h2 className="inspector-title">Are these the same {suggestion.a.kind}?</h2>

      <div className="versus">
        {[suggestion.a, suggestion.b].map((side, i) => {
          const other = i === 0 ? suggestion.b : suggestion.a;
          return (
            <section
              className="versus-side"
              key={side.id}
              aria-label={`Entity ${i === 0 ? "A" : "B"}`}
            >
              <div className="entity-head">
                <span className="monogram" data-kind={side.kind} aria-hidden>
                  {initials(side.name)}
                </span>
                <div className="min0">
                  <p className="versus-statement">{side.name}</p>
                  <KindTag kind={side.kind} />
                </div>
              </div>
              <EntityFacts id={side.id} />
              <Button
                variant="secondary"
                icon={Check}
                className="versus-keep"
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
  );
}

export default function Entities() {
  const [items, setItems] = useState<MergeSuggestion[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
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

  useEffect(() => {
    if (!items || items.length === 0) return;
    if (!selected || !items.some((s) => keyOf(s) === selected)) setSelected(keyOf(items[0]));
  }, [items, selected]);

  const listRef = useListKeys(items ?? [], selected, keyOf, setSelected);
  const current = items?.find((s) => keyOf(s) === selected);

  return (
    <>
      <Toolbar
        title="Entities"
        icon={Combine}
        count={items && items.length > 0 ? items.length : undefined}
      />
      {error && (
        <div className="view-callout">
          <Callout title="Couldn't load suggestions">{error}</Callout>
        </div>
      )}
      {items !== null && items.length === 0 && !error ? (
        <EmptyState icon={CircleCheck} tone="success" title="No duplicates to check">
          Your knowledge graph looks deduplicated. Clear matches merge on their own; close calls
          appear here.
        </EmptyState>
      ) : (
        <SplitView
          listLabel="Possible duplicates"
          listHeader={
            <p className="hint list-note">Close calls only. Clear matches merge on their own.</p>
          }
          list={
            items === null ? (
              <Skeleton rows={4} />
            ) : (
              <ul className="rows" ref={listRef}>
                {items.map((s) => (
                  <li key={keyOf(s)}>
                    <button
                      type="button"
                      className="row"
                      aria-current={keyOf(s) === selected ? "true" : undefined}
                      onClick={() => setSelected(keyOf(s))}
                    >
                      <span className="monogram monogram-sm" data-kind={s.a.kind} aria-hidden>
                        {initials(s.a.name)}
                      </span>
                      <span className="row-main">
                        <span className="row-title">
                          {s.a.name} <span className="approx">≈</span> {s.b.name}
                        </span>
                        <span className="row-meta">
                          <span className="cap">{s.a.kind}</span>
                          <span className="dot-sep">{METHOD_LABELS[s.method] ?? s.method}</span>
                        </span>
                      </span>
                      <span className="row-trail">
                        <span className="num row-score">{s.score.toFixed(2)}</span>
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )
          }
          detail={
            current ? (
              <Detail
                suggestion={current}
                onDone={() => {
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
