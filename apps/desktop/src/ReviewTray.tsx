import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  Check,
  CircleCheck,
  Combine,
  EyeOff,
  Inbox,
  Layers,
  Link2,
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
  IconButton,
  Kbd,
  Meter,
  Skeleton,
  SplitView,
  Toolbar,
  When,
  errorText,
  type Tone,
} from "./ui";

const TRAY_LIMIT = 100;
/** Re-read the tray this often: background workers park new items. */
const REFRESH_MS = 15000;

const REASONS: Record<string, { label: string; tone: Tone; icon: typeof Layers; lead: string }> = {
  "low-confidence": {
    label: "Unsure fact",
    tone: "warning",
    icon: ShieldQuestion,
    lead: "lead-warning",
  },
  "merge-band": { label: "Possible duplicate", tone: "info", icon: Combine, lead: "lead-info" },
  "oversized-component": {
    label: "Large duplicate group",
    tone: "neutral",
    icon: Layers,
    lead: "",
  },
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

function reasonOf(item: ReviewItem) {
  return (
    REASONS[item.reason] ?? { label: item.reason, tone: "neutral" as Tone, icon: Layers, lead: "" }
  );
}

/** One line naming the item, for the list. */
function titleOf(item: ReviewItem): string {
  if (item.target_kind === "unit") return item.statement ?? "(statement unavailable)";
  if (item.reason === "merge-band") {
    const a = asString(item.signals.a);
    const b = asString(item.signals.b);
    return `${item.a_name ?? a?.slice(0, 8) ?? "?"} ≈ ${item.b_name ?? b?.slice(0, 8) ?? "?"}`;
  }
  const members = Array.isArray(item.signals.members) ? item.signals.members.length : 0;
  return `${plural(members, "entity", "entities")} chained together`;
}

/** Oversized duplicate groups can only be dismissed: there is no single
 *  pair to accept or reject, and the server refuses a wholesale action. */
function canJudge(item: ReviewItem): boolean {
  return item.reason !== "oversized-component";
}

function isTyping(target: EventTarget | null): boolean {
  return target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement;
}

/** The selected item, large, with everything needed to decide. */
function ReviewDetail({
  item,
  busy,
  editing,
  draft,
  onDraft,
  onEdit,
  onCancelEdit,
  onSave,
  onAccept,
  onReject,
  onDismiss,
}: {
  item: ReviewItem;
  busy: boolean;
  editing: boolean;
  draft: string;
  onDraft: (s: string) => void;
  onEdit: () => void;
  onCancelEdit: () => void;
  onSave: () => void;
  onAccept: () => void;
  onReject: () => void;
  onDismiss: () => void;
}) {
  const reason = reasonOf(item);
  const isUnit = item.target_kind === "unit";
  const confidence = asNumber(item.signals.confidence);
  const score = asNumber(item.signals.score);
  const chained = item.signals.chained === true;

  return (
    <div className="inspector review-detail" key={item.id}>
      <div className="inspector-kicker">
        <Badge tone={reason.tone} icon={reason.icon}>
          {reason.label}
        </Badge>
        <span className="hint">
          parked <When iso={item.created_at} />
        </span>
      </div>

      {editing ? (
        <div className="edit-block">
          <label className="field-label" htmlFor="review-edit">
            Corrected statement
          </label>
          <textarea
            id="review-edit"
            className="input textarea"
            autoFocus
            rows={3}
            value={draft}
            onChange={(e) => onDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && draft.trim()) {
                e.preventDefault();
                onSave();
              }
              if (e.key === "Escape") onCancelEdit();
            }}
          />
        </div>
      ) : item.reason === "merge-band" ? (
        <div className="pair-hero">
          <span className="pair-name">{item.a_name ?? "?"}</span>
          <span className="pair-approx" aria-label="might be the same as">
            ≈
          </span>
          <span className="pair-name">{item.b_name ?? "?"}</span>
        </div>
      ) : (
        <p className="statement-hero">{titleOf(item)}</p>
      )}

      <dl className="facts">
        {confidence !== null && (
          <div>
            <dt>Confidence</dt>
            <dd>
              <Meter value={confidence} label="Confidence" tone="warning" width={120} />
            </dd>
          </div>
        )}
        {score !== null && (
          <div>
            <dt>Similarity</dt>
            <dd>
              <Meter value={score} label="Similarity" tone="info" width={120} />
            </dd>
          </div>
        )}
        <div>
          <dt>Information gain</dt>
          <dd>
            <Meter value={item.info_gain} label="Information gain" tone="neutral" width={120} />
          </dd>
        </div>
      </dl>

      {chained && (
        <Callout tone="info" icon={Link2} title="Part of a chain">
          This pair is a close match, but it links to things that don't match each other, so Gather
          didn't merge any of them on its own. Merge the pairs that are right.
        </Callout>
      )}
      {item.reason === "oversized-component" && (
        <Callout tone="neutral" icon={Layers}>
          Too many names are linked together to merge safely in one go. Dismiss it once you've
          looked; the individual pairs still come through on their own.
        </Callout>
      )}
      {item.reason === "low-confidence" && (
        <p className="explain">
          Gather wasn't sure about this statement. It's already in your library; keeping or removing
          it teaches Gather where its bar should sit.
        </p>
      )}

      <div className="inspector-actions">
        {editing ? (
          <>
            <Button
              variant="primary"
              icon={Check}
              onClick={onSave}
              disabled={busy || !draft.trim()}
              shortcut="↵"
            >
              Save
            </Button>
            <Button variant="ghost" onClick={onCancelEdit} shortcut="Esc">
              Cancel
            </Button>
          </>
        ) : (
          <>
            {canJudge(item) && (
              <>
                <Button
                  variant="primary"
                  icon={isUnit ? Check : Combine}
                  onClick={onAccept}
                  disabled={busy}
                  shortcut="A"
                >
                  {isUnit ? "Keep" : "Merge"}
                </Button>
                <Button
                  variant="secondary"
                  icon={isUnit ? Trash2 : Unlink}
                  onClick={onReject}
                  disabled={busy}
                  shortcut="R"
                >
                  {isUnit ? "Remove" : "Not duplicates"}
                </Button>
              </>
            )}
            {isUnit && (
              <Button variant="ghost" icon={Pencil} onClick={onEdit} disabled={busy} shortcut="E">
                Edit
              </Button>
            )}
            <span className="spacer" />
            <Button variant="ghost" icon={EyeOff} onClick={onDismiss} disabled={busy} shortcut="D">
              Dismiss
            </Button>
          </>
        )}
      </div>
    </div>
  );
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
      ?.querySelector('[aria-current="true"]')
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

  const toolbar = (
    <Toolbar title="Review" icon={Inbox} count={items.length > 0 ? items.length : undefined}>
      <span className="toolbar-keys" aria-label="Keyboard shortcuts">
        <Kbd>J</Kbd>
        <Kbd>K</Kbd>
        <span>move</span>
        <Kbd>U</Kbd>
        <span>undo</span>
      </span>
      <IconButton icon={RefreshCw} label="Refresh" size="sm" onClick={reload} disabled={busy} />
    </Toolbar>
  );

  if (tray.loading && !tray.data) {
    return (
      <>
        {toolbar}
        <SplitView listLabel="Review items" list={<Skeleton rows={6} />} detail={null} />
      </>
    );
  }
  if (tray.error) {
    return (
      <>
        {toolbar}
        <div className="view-callout">
          <Callout title="Couldn't load the review tray">{tray.error}</Callout>
        </div>
      </>
    );
  }

  return (
    <>
      {toolbar}
      {items.length === 0 ? (
        <EmptyState icon={CircleCheck} tone="success" title="Nothing needs you">
          The pipeline decided everything on its own. Items appear here only when Gather is
          genuinely unsure. Everything here is optional; answering only tunes Gather's thresholds.
        </EmptyState>
      ) : (
        <SplitView
          listLabel="Review items"
          listHeader={
            <p className="hint list-note">
              Optional. Everything here is already live; your answers tune Gather's thresholds.
            </p>
          }
          list={
            <ul className="rows" ref={listRef}>
              {items.map((item, index) => {
                const reason = reasonOf(item);
                const Icon = reason.icon;
                const isCurrent = item === current;
                return (
                  <li key={item.id}>
                    <button
                      type="button"
                      className="row"
                      aria-current={isCurrent ? "true" : undefined}
                      onClick={() => setSelected(index)}
                    >
                      <span className={`row-lead ${reason.lead}`} aria-hidden>
                        <Icon />
                      </span>
                      <span className="row-main">
                        <span className="row-title wrap">{titleOf(item)}</span>
                        <span className="row-meta">
                          {reason.label}
                          {item.signals.chained === true && (
                            <span className="dot-sep">chained</span>
                          )}
                        </span>
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          }
          detail={
            <>
              {error && (
                <div className="view-callout">
                  <Callout>{error}</Callout>
                </div>
              )}
              {current && (
                <ReviewDetail
                  item={current}
                  busy={busy}
                  editing={editingId === current.id}
                  draft={draft}
                  onDraft={setDraft}
                  onEdit={() => {
                    setEditingId(current.id);
                    setDraft(current.statement ?? "");
                  }}
                  onCancelEdit={() => setEditingId(null)}
                  onSave={() => saveEdit(current)}
                  onAccept={() => accept(current)}
                  onReject={() => reject(current)}
                  onDismiss={() => dismiss(current)}
                />
              )}
            </>
          }
        />
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
    </>
  );
}
