import { useCallback, useEffect, useState } from "react";
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

function ProvenanceList({ items }: { items: Provenance[] }) {
  if (items.length === 0) return <p className="prov-empty">no provenance recorded</p>;
  return (
    <ul className="prov-list">
      {items.map((p, i) => (
        <li key={i}>
          <span className="prov-badge">{p.source_platform}</span>
          <span className="prov-kind">{p.artifact_kind}</span>
          {p.original_filename && <span className="prov-file">{p.original_filename}</span>}
          <span className="prov-time">{new Date(p.ingested_at).toLocaleString()}</span>
          {p.quote && <blockquote>“{p.quote}”</blockquote>}
        </li>
      ))}
    </ul>
  );
}

function Detail({
  id,
  onResolved,
}: {
  id: string;
  onResolved: () => void;
}) {
  const [detail, setDetail] = useState<ContradictionDetail | null>(null);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    getContradiction(id).then(setDetail).catch((e) => setError(String(e)));
  }, [id]);
  useEffect(reload, [reload]);

  const act = async (resolution: Resolution) => {
    setBusy(true);
    setError(null);
    try {
      await resolveContradiction(id, resolution, note.trim() || undefined);
      onResolved();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const annotate = async () => {
    if (!note.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await annotateContradiction(id, note.trim());
      setNote("");
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (!detail) return <p>loading…</p>;

  return (
    <div className="conflict-detail">
      <div className="conflict-sides">
        {(["unit_a", "unit_b"] as const).map((side) => (
          <div className="conflict-side" key={side}>
            <h4>{side === "unit_a" ? "Statement A" : "Statement B"}</h4>
            <p className="statement">{detail[side].statement}</p>
            {detail[side].valid_from && (
              <p className="valid-from">
                since {new Date(detail[side].valid_from!).toLocaleDateString()}
              </p>
            )}
            <ProvenanceList items={detail[side].provenance} />
          </div>
        ))}
      </div>

      {detail.explanation && <p className="explanation">{detail.explanation}</p>}

      <div className="conflict-actions">
        <input
          type="text"
          placeholder="optional note…"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          disabled={busy}
        />
        <button disabled={busy} onClick={() => act("resolved_a")}>Keep A</button>
        <button disabled={busy} onClick={() => act("resolved_b")}>Keep B</button>
        <button disabled={busy} onClick={() => act("both_valid")}>Both valid</button>
        <button disabled={busy} onClick={() => act("dismissed")}>Dismiss</button>
        <button disabled={busy || !note.trim()} onClick={annotate}>Annotate</button>
      </div>

      {detail.audit.length > 0 && (
        <details className="audit">
          <summary>history ({detail.audit.length})</summary>
          <ul>
            {detail.audit.map((a, i) => (
              <li key={i}>
                {new Date(a.created_at).toLocaleString()} — {a.actor}: {a.action}
                {a.note ? ` — ${a.note}` : ""}
              </li>
            ))}
          </ul>
        </details>
      )}

      {error && <p className="error">{error}</p>}
    </div>
  );
}

export default function Contradictions() {
  const [items, setItems] = useState<ContradictionSummary[]>([]);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    listContradictions("open")
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
    <section className="contradictions">
      {error && <p className="error">{error}</p>}
      {items.length === 0 && !error && (
        <p className="all-clear">No open contradictions — your knowledge base agrees with itself.</p>
      )}
      <ul className="conflict-list">
        {items.map((c) => (
          <li key={c.id} className="conflict-item">
            <button
              className="conflict-row"
              onClick={() => setExpanded(expanded === c.id ? null : c.id)}
            >
              <span className="score">{c.score.toFixed(2)}</span>
              <span className="statements">
                <span>{c.unit_a.statement}</span>
                <span className="vs">vs</span>
                <span>{c.unit_b.statement}</span>
              </span>
              <span className="method">{c.detection_method}</span>
            </button>
            {expanded === c.id && (
              <Detail
                id={c.id}
                onResolved={() => {
                  setExpanded(null);
                  refresh();
                }}
              />
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
