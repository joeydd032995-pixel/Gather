import { useCallback, useEffect, useState } from "react";
import {
  acceptReview,
  dismissReview,
  editUnit,
  getEntity,
  listReview,
  rejectReview,
  restoreUnit,
  type ReviewItem,
} from "./api";
import { useAsync } from "./hooks/useAsync";

const TRAY_LIMIT = 100;

const REASON_LABELS: Record<string, string> = {
  "low-confidence": "Low-confidence fact",
  "merge-band": "Possible duplicate",
  "oversized-component": "Large duplicate group",
};

/** The last action, and how to take it back when that is possible. */
interface LastAction {
  label: string;
  undo?: () => Promise<unknown>;
}

function EntityName({ id }: { id: string }) {
  const [name, setName] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    getEntity(id)
      .then((e) => {
        if (!cancelled) setName(e.name);
      })
      .catch(() => {
        if (!cancelled) setName(id.slice(0, 8));
      });
    return () => {
      cancelled = true;
    };
  }, [id]);
  return <strong>{name ?? "…"}</strong>;
}

function asString(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function asNumber(value: unknown): number | null {
  return typeof value === "number" ? value : null;
}

function ItemSummary({ item }: { item: ReviewItem }) {
  if (item.target_kind === "unit") {
    const confidence = asNumber(item.signals.confidence);
    return (
      <span>
        {item.statement ?? "(statement unavailable)"}
        {confidence !== null && (
          <span className="method"> · confidence {confidence.toFixed(2)}</span>
        )}
      </span>
    );
  }
  if (item.reason === "merge-band") {
    const a = asString(item.signals.a);
    const b = asString(item.signals.b);
    const score = asNumber(item.signals.score);
    if (!a || !b) return <span>Malformed merge suggestion</span>;
    return (
      <span>
        <EntityName id={a} /> ↔ <EntityName id={b} />
        {score !== null && <span className="method"> · similarity {score.toFixed(2)}</span>}
      </span>
    );
  }
  const members = Array.isArray(item.signals.members) ? item.signals.members.length : 0;
  return <span>{members} entities chained together; too large to merge automatically</span>;
}

function isTyping(target: EventTarget | null): boolean {
  return target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement;
}

/**
 * The optional review tray: only what the pipeline could not decide, most
 * informative first. Everything here is already live; answering just teaches
 * the thresholds. Keys: j/k move, a accept, r reject, d dismiss, e edit, u undo.
 */
export default function ReviewTray() {
  const tray = useAsync(() => listReview(TRAY_LIMIT), []);
  const items = tray.data ?? [];
  const [selected, setSelected] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [last, setLast] = useState<LastAction | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");

  const { reload } = tray;
  const run = useCallback(
    async (action: () => Promise<LastAction>) => {
      setBusy(true);
      setError(null);
      try {
        setLast(await action());
        reload();
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(false);
      }
    },
    [reload],
  );

  const accept = useCallback(
    (item: ReviewItem) =>
      run(async () => {
        await acceptReview(item.id);
        return { label: item.target_kind === "unit" ? "Kept" : "Merged" };
      }),
    [run],
  );

  const reject = useCallback(
    (item: ReviewItem) =>
      run(async () => {
        await rejectReview(item.id);
        if (item.target_kind === "unit") {
          return { label: "Removed from the brain", undo: () => restoreUnit(item.target_id) };
        }
        return { label: "Marked as not duplicates" };
      }),
    [run],
  );

  const dismiss = useCallback(
    (item: ReviewItem) =>
      run(async () => {
        await dismissReview(item.id);
        return { label: "Dismissed" };
      }),
    [run],
  );

  const undo = useCallback(() => {
    const undoAction = last?.undo;
    if (!undoAction) return;
    run(async () => {
      await undoAction();
      return { label: "Undone" };
    });
  }, [last, run]);

  const saveEdit = useCallback(
    (item: ReviewItem) =>
      run(async () => {
        await editUnit(item.target_id, draft.trim());
        setEditingId(null);
        return { label: "Corrected" };
      }),
    [draft, run],
  );

  const current = items[Math.min(selected, items.length - 1)];

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (busy || isTyping(e.target) || e.metaKey || e.ctrlKey) return;
      const key = e.key.toLowerCase();
      if (key === "j" || key === "arrowdown") {
        setSelected((s) => Math.min(s + 1, Math.max(items.length - 1, 0)));
      } else if (key === "k" || key === "arrowup") {
        setSelected((s) => Math.max(s - 1, 0));
      } else if (key === "u") {
        undo();
      } else if (current && key === "a") {
        accept(current);
      } else if (current && key === "r") {
        reject(current);
      } else if (current && key === "d") {
        dismiss(current);
      } else if (current && key === "e" && current.target_kind === "unit") {
        e.preventDefault();
        setEditingId(current.id);
        setDraft(current.statement ?? "");
      } else {
        return;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [accept, busy, current, dismiss, items.length, reject, undo]);

  if (tray.loading && !tray.data) return <p>Loading review tray…</p>;
  if (tray.error) return <p className="error">{tray.error}</p>;

  return (
    <section>
      <p className="hint">
        Optional. Everything here is already live in your brain; answering only
        teaches Gather where its thresholds should sit. Keys: <kbd>j</kbd>/<kbd>k</kbd>{" "}
        move, <kbd>a</kbd> accept, <kbd>r</kbd> reject, <kbd>d</kbd> dismiss,{" "}
        <kbd>e</kbd> edit, <kbd>u</kbd> undo.
      </p>
      {last && (
        <div className="toast" role="status">
          {last.label}
          {last.undo && (
            <button onClick={undo} disabled={busy}>
              Undo
            </button>
          )}
        </div>
      )}
      {error && <p className="error">{error}</p>}
      {items.length === 0 ? (
        <p className="all-clear">Nothing needs you. The pipeline decided everything.</p>
      ) : (
        <ul className="conflict-list">
          {items.map((item, index) => (
            <li
              key={item.id}
              className={`conflict-item ${item === current ? "selected" : ""}`}
              onClick={() => setSelected(index)}
            >
              <div className="conflict-row">
                <span className="prov-badge">{REASON_LABELS[item.reason] ?? item.reason}</span>
                <span className="statements">
                  <ItemSummary item={item} />
                </span>
                <span className="method" title="information gain: how much one answer teaches">
                  {item.info_gain.toFixed(2)}
                </span>
              </div>
              {editingId === item.id ? (
                <div className="conflict-actions">
                  <input
                    autoFocus
                    value={draft}
                    onChange={(e) => setDraft(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && draft.trim()) saveEdit(item);
                      if (e.key === "Escape") setEditingId(null);
                    }}
                  />
                  <button onClick={() => saveEdit(item)} disabled={busy || !draft.trim()}>
                    Save
                  </button>
                  <button onClick={() => setEditingId(null)}>Cancel</button>
                </div>
              ) : (
                <div className="conflict-actions">
                  {item.reason !== "oversized-component" && (
                    <button onClick={() => accept(item)} disabled={busy}>
                      {item.target_kind === "unit" ? "Keep" : "Merge"}
                    </button>
                  )}
                  <button onClick={() => reject(item)} disabled={busy}>
                    {item.target_kind === "unit" ? "Remove" : "Not duplicates"}
                  </button>
                  {item.target_kind === "unit" && (
                    <button
                      onClick={() => {
                        setEditingId(item.id);
                        setDraft(item.statement ?? "");
                      }}
                      disabled={busy}
                    >
                      Edit
                    </button>
                  )}
                  <button onClick={() => dismiss(item)} disabled={busy}>
                    Dismiss
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
