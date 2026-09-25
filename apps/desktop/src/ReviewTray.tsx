import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  Check,
  CircleCheck,
  Combine,
  EyeOff,
  Layers,
  Pencil,
  RefreshCw,
  ShieldQuestion,
  Trash2,
  Undo2,
  Unlink,
} from "lucide-react";
import {
  acceptReview,
  dismissReview,
  editUnit,
  listReview,
  rejectReview,
  restoreUnit,
  type ReviewItem,
} from "./api";
import { useAsync } from "./hooks/useAsync";
import { plural } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Kbd,
  Meter,
  PageHeader,
  Skeleton,
  errorText,
  type Tone,
} from "./ui";

const TRAY_LIMIT = 100;
/** Re-read the tray this often: background workers park new items. */
const REFRESH_MS = 15000;

const REASONS: Record<string, { label: string; tone: Tone; icon: typeof Layers }> = {
  "low-confidence": { label: "Unsure fact", tone: "warning", icon: ShieldQuestion },
  "merge-band": { label: "Possible duplicate", tone: "info", icon: Combine },
  "oversized-component": { label: "Large duplicate group", tone: "neutral", icon: Layers },
};

/** The last action, and how to take it back when that is possible. */
interface LastAction {
  label: string;
  undo?: () => Promise<unknown>;
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
      <>
        <p className="item-title">{item.statement ?? "(statement unavailable)"}</p>
        {confidence !== null && (
          <div className="item-sub">
            <span>Confidence</span>
            <Meter value={confidence} label="Confidence" tone="warning" width={44} />
          </div>
        )}
      </>
    );
  }
  if (item.reason === "merge-band") {
    const a = asString(item.signals.a);
    const b = asString(item.signals.b);
    const score = asNumber(item.signals.score);
    if (!a || !b) return <p className="item-title">Malformed merge suggestion</p>;
    // Names come with the tray listing; an id means the entity is gone.
    return (
      <>
        <p className="pair-names">
          <span className="item-title">{item.a_name ?? a.slice(0, 8)}</span>
          <span className="pair-sep" aria-label="and">
            ≈
          </span>
          <span className="item-title">{item.b_name ?? b.slice(0, 8)}</span>
        </p>
        {score !== null && (
          <div className="item-sub">
            <span>Similarity</span>
            <Meter value={score} label="Similarity" tone="info" width={44} />
          </div>
        )}
      </>
    );
  }
  const members = Array.isArray(item.signals.members) ? item.signals.members.length : 0;
  return (
    <>
      <p className="item-title">{plural(members, "entity", "entities")} chained together</p>
      <div className="item-sub">Too large to merge automatically; dismiss once you've looked.</div>
    </>
  );
}

/** Oversized duplicate groups can only be dismissed: there is no single
 *  pair to accept or reject, and the server refuses a wholesale action. */
function canJudge(item: ReviewItem): boolean {
  return item.reason !== "oversized-component";
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
  const listRef = useRef<HTMLUListElement>(null);

  const { reload } = tray;
  const run = useCallback(
    async (action: () => Promise<LastAction>) => {
      setBusy(true);
      setError(null);
      try {
        setLast(await action());
        reload();
      } catch (e) {
        setError(errorText(e));
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
      if (busy || isTyping(e.target) || e.metaKey || e.ctrlKey || e.altKey) return;
      if (document.querySelector('[role="dialog"]')) return;
      const key = e.key.toLowerCase();
      if (key === "j" || key === "arrowdown") {
        e.preventDefault();
        setSelected((s) => Math.min(s + 1, Math.max(items.length - 1, 0)));
      } else if (key === "k" || key === "arrowup") {
        e.preventDefault();
        setSelected((s) => Math.max(s - 1, 0));
      } else if (key === "u") {
        undo();
      } else if (current && key === "a" && canJudge(current)) {
        accept(current);
      } else if (current && key === "r" && canJudge(current)) {
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

  // Keep the selected item in view as j/k move through a long tray.
  useEffect(() => {
    listRef.current
      ?.querySelector(".item.selected")
      ?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, [selected]);

  useEffect(() => {
    const timer = setInterval(() => {
      if (!busy && editingId === null) reload();
    }, REFRESH_MS);
    return () => clearInterval(timer);
  }, [busy, editingId, reload]);

  // The confirmation fades on its own unless it still offers an undo.
  useEffect(() => {
    if (!last || last.undo) return;
    const timer = setTimeout(() => setLast(null), 3500);
    return () => clearTimeout(timer);
  }, [last]);

  const header = (
    <PageHeader
      title="Review"
      description="Optional. Everything here is already live in your library; answering only teaches Gather where its thresholds should sit."
      eyebrow={items.length > 0 ? plural(items.length, "item") + " waiting" : undefined}
      actions={
        <Button variant="ghost" size="sm" icon={RefreshCw} onClick={reload} disabled={busy}>
          Refresh
        </Button>
      }
    />
  );

  if (tray.loading && !tray.data) {
    return (
      <section>
        {header}
        <Skeleton rows={4} variant="card" />
      </section>
    );
  }
  if (tray.error) {
    return (
      <section>
        {header}
        <Callout title="Couldn't load the review tray">{tray.error}</Callout>
      </section>
    );
  }

  return (
    <section>
      {header}

      {items.length > 0 && (
        <div className="keys" aria-label="Keyboard shortcuts">
          <span>
            <Kbd>J</Kbd>
            <Kbd>K</Kbd> move
          </span>
          <span>
            <Kbd>A</Kbd> keep / merge
          </span>
          <span>
            <Kbd>R</Kbd> remove
          </span>
          <span>
            <Kbd>E</Kbd> edit
          </span>
          <span>
            <Kbd>D</Kbd> dismiss
          </span>
          <span>
            <Kbd>U</Kbd> undo
          </span>
        </div>
      )}

      {error && <Callout>{error}</Callout>}

      {items.length === 0 ? (
        <EmptyState icon={CircleCheck} tone="success" title="Nothing needs you">
          The pipeline decided everything on its own. Items appear here only when Gather is
          genuinely unsure.
        </EmptyState>
      ) : (
        <ul className="stack" ref={listRef}>
          {items.map((item, index) => {
            const reason = REASONS[item.reason] ?? {
              label: item.reason,
              tone: "neutral" as Tone,
              icon: Layers,
            };
            const isCurrent = item === current;
            const isUnit = item.target_kind === "unit";
            return (
              <li
                key={item.id}
                className={`item review-item${isCurrent ? " selected" : ""}`}
                onClick={() => setSelected(index)}
                aria-current={isCurrent ? "true" : undefined}
              >
                <div className="item-row">
                  <div className="item-main">
                    <Badge tone={reason.tone} icon={reason.icon} className="review-reason">
                      {reason.label}
                    </Badge>
                    {editingId === item.id ? (
                      <div className="edit-row">
                        <input
                          className="input"
                          autoFocus
                          aria-label="Corrected statement"
                          value={draft}
                          onChange={(e) => setDraft(e.target.value)}
                          onKeyDown={(e) => {
                            if (e.key === "Enter" && draft.trim()) saveEdit(item);
                            if (e.key === "Escape") setEditingId(null);
                          }}
                        />
                        <Button
                          variant="primary"
                          size="sm"
                          icon={Check}
                          onClick={() => saveEdit(item)}
                          disabled={busy || !draft.trim()}
                        >
                          Save
                        </Button>
                        <Button variant="ghost" size="sm" onClick={() => setEditingId(null)}>
                          Cancel
                        </Button>
                      </div>
                    ) : (
                      <ItemSummary item={item} />
                    )}
                  </div>
                  <div className="item-aside review-gain">
                    <span className="gain-label" aria-hidden>
                      Info gain
                    </span>
                    <Meter
                      value={item.info_gain}
                      label="Information gain: how much one answer teaches"
                      tone="neutral"
                      width={40}
                    />
                  </div>
                </div>
                {editingId !== item.id && (
                  <div className="item-foot">
                    {canJudge(item) && (
                      <>
                        <Button
                          variant={isCurrent ? "primary" : "secondary"}
                          size="sm"
                          icon={isUnit ? Check : Combine}
                          onClick={() => accept(item)}
                          disabled={busy}
                          shortcut={isCurrent ? "A" : undefined}
                        >
                          {isUnit ? "Keep" : "Merge"}
                        </Button>
                        <Button
                          variant="secondary"
                          size="sm"
                          icon={isUnit ? Trash2 : Unlink}
                          onClick={() => reject(item)}
                          disabled={busy}
                          shortcut={isCurrent ? "R" : undefined}
                        >
                          {isUnit ? "Remove" : "Not duplicates"}
                        </Button>
                      </>
                    )}
                    {isUnit && (
                      <Button
                        variant="ghost"
                        size="sm"
                        icon={Pencil}
                        onClick={() => {
                          setEditingId(item.id);
                          setDraft(item.statement ?? "");
                        }}
                        disabled={busy}
                      >
                        Edit
                      </Button>
                    )}
                    <Button
                      variant="ghost"
                      size="sm"
                      icon={EyeOff}
                      onClick={() => dismiss(item)}
                      disabled={busy}
                    >
                      Dismiss
                    </Button>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}

      {createPortal(
        <div className="toast-region" aria-live="polite">
          {last && (
            <div className="toast" role="status" key={last.label + String(!!last.undo)}>
              <CircleCheck aria-hidden />
              <span>{last.label}</span>
              {last.undo && (
                <Button size="sm" icon={Undo2} onClick={undo} disabled={busy} shortcut="U">
                  Undo
                </Button>
              )}
            </div>
          )}
        </div>,
        document.body,
      )}
    </section>
  );
}
