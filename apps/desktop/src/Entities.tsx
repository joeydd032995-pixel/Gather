import { useCallback, useEffect, useState } from "react";
import {
  dismissMergeSuggestion,
  getEntity,
  listMergeSuggestions,
  mergeEntities,
  type EntityDetail,
  type EntityRef,
  type MergeSuggestion,
} from "./api";

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

  if (!detail) return null;
  return (
    <ul className="prov-list">
      <li>
        <span className="prov-kind">{detail.kind}</span>
        <span className="prov-time">
          since {new Date(detail.created_at).toLocaleDateString()}
        </span>
      </li>
      {detail.aliases.length > 0 && (
        <li>
          {detail.aliases.map((a) => (
            <span className="prov-badge" key={a}>
              {a}
            </span>
          ))}
        </li>
      )}
      {detail.description && <li>{detail.description}</li>}
    </ul>
  );
}

function Detail({
  suggestion,
  onDone,
}: {
  suggestion: MergeSuggestion;
  onDone: () => void;
}) {
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const merge = (winner: EntityRef, loser: EntityRef) =>
    run(() => mergeEntities(winner.id, loser.id, note.trim() || undefined));

  return (
    <div className="conflict-detail">
      <div className="conflict-sides">
        {[suggestion.a, suggestion.b].map((side, i) => (
          <div className="conflict-side" key={side.id}>
            <h4>{i === 0 ? "Entity A" : "Entity B"}</h4>
            <p className="statement">{side.name}</p>
            <EntityFacts id={side.id} />
          </div>
        ))}
      </div>

      <p className="explanation">
        Merging keeps one entity and folds the other into it as an alias — its
        units, relationships and provenance move across, and the name it was
        known by keeps resolving to the survivor.
      </p>

      <div className="conflict-actions">
        <input
          type="text"
          placeholder="optional note…"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          disabled={busy}
        />
        <button disabled={busy} onClick={() => merge(suggestion.a, suggestion.b)}>
          Keep A
        </button>
        <button disabled={busy} onClick={() => merge(suggestion.b, suggestion.a)}>
          Keep B
        </button>
        <button
          disabled={busy}
          onClick={() =>
            run(() =>
              dismissMergeSuggestion(
                suggestion.a.id,
                suggestion.b.id,
                note.trim() || undefined,
              ),
            )
          }
        >
          Not duplicates
        </button>
      </div>

      {error && <p className="error">{error}</p>}
    </div>
  );
}

export default function Entities() {
  const [items, setItems] = useState<MergeSuggestion[]>([]);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    listMergeSuggestions()
      .then((list) => {
        setItems(list);
        setError(null);
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  useEffect(() => {
    refresh();
    const timer = setInterval(refresh, 15000);
    return () => clearInterval(timer);
  }, [refresh]);

  return (
    <section className="entities">
      {error && <p className="error">{error}</p>}
      {items.length === 0 && !error && (
        <p className="all-clear">
          No duplicate entities suggested — your knowledge graph looks deduplicated.
        </p>
      )}
      <ul className="conflict-list">
        {items.map((s) => {
          const key = `${s.a.id}:${s.b.id}`;
          return (
            <li key={key} className="conflict-item">
              <button
                className="conflict-row"
                onClick={() => setExpanded(expanded === key ? null : key)}
              >
                <span className="score">{s.score.toFixed(2)}</span>
                <span className="statements">
                  <span>{s.a.name}</span>
                  <span className="vs">vs</span>
                  <span>{s.b.name}</span>
                </span>
                <span className="method">{s.method}</span>
              </button>
              {expanded === key && (
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
    </section>
  );
}
