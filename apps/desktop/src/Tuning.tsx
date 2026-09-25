import { useState } from "react";
import { ChevronRight, History, PauseCircle, RotateCcw, Sparkles } from "lucide-react";
import { getTuning, resetTuning, type TunedThreshold, type TuningKey } from "./api";
import { useAsync } from "./hooks/useAsync";
import { Badge, Button, Callout, PageHeader, Skeleton, When, errorText } from "./ui";

const KEY_LABELS: Record<TuningKey, { title: string; desc: string }> = {
  "admit.hold_below": {
    title: "Review threshold",
    desc: "Facts below this confidence go to the review tray.",
  },
  "merge.auto_single": {
    title: "Auto-merge similarity",
    desc: "Duplicates at or above this similarity merge automatically.",
  },
  "merge.agree": {
    title: "Agreement merge",
    desc: "…or when name and meaning both agree at least this much.",
  },
};

/** Where the value sits within its allowed range, with the default marked. */
function RangeTrack({ t }: { t: TunedThreshold }) {
  const span = t.bounds.max - t.bounds.min || 1;
  const pos = (v: number) => `${Math.max(0, Math.min(1, (v - t.bounds.min) / span)) * 100}%`;
  return (
    <div className="range" aria-hidden>
      <div className="range-track">
        <span
          className="range-fill"
          style={{
            left: pos(Math.min(t.value, t.default)),
            width: `calc(${pos(Math.max(t.value, t.default))} - ${pos(Math.min(t.value, t.default))})`,
          }}
        />
        <span className="range-default" style={{ left: pos(t.default) }} title="default" />
        <span
          className={t.tuned ? "range-thumb tuned" : "range-thumb"}
          style={{ left: pos(t.value) }}
        />
      </div>
      <div className="range-scale num">
        <span>{t.bounds.min.toFixed(2)}</span>
        <span>{t.bounds.max.toFixed(2)}</span>
      </div>
    </div>
  );
}

/** What the pipeline learned from your answers, why, and how to undo it. */
export default function Tuning() {
  const tuning = useAsync(getTuning, []);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const reset = async (key?: TuningKey) => {
    setBusy(key ?? "all");
    setError(null);
    try {
      await resetTuning(key);
      tuning.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const state = tuning.data;
  const anyTuned = state?.thresholds.some((t) => t.tuned) ?? false;

  const header = (
    <PageHeader
      title="Tuning"
      description="Gather adjusts its own thresholds from your review answers. Here's what it learned, why, and how to undo it."
      actions={
        anyTuned && (
          <Button
            variant="secondary"
            size="sm"
            icon={RotateCcw}
            onClick={() => reset()}
            loading={busy === "all"}
            disabled={busy !== null}
          >
            Reset all
          </Button>
        )
      }
    />
  );

  if (tuning.loading && !state) {
    return (
      <section>
        {header}
        <Skeleton rows={3} variant="card" />
      </section>
    );
  }
  if (tuning.error || !state) {
    return (
      <section>
        {header}
        <Callout title="Couldn't load tuning">{tuning.error}</Callout>
      </section>
    );
  }

  return (
    <section>
      {header}
      {state.enabled ? (
        <Callout tone="accent" icon={Sparkles}>
          Gather tightens a threshold when fewer than{" "}
          <strong>{Math.round(state.target_precision * 100)}%</strong> of auto-accepted items hold
          up, and loosens it only on strong evidence (at least <strong>{state.min_samples}</strong>{" "}
          answers).
        </Callout>
      ) : (
        <Callout tone="warning" icon={PauseCircle} title="Auto-tuning is paused">
          GATHER_TUNE_ENABLED=false: the thresholds below are frozen at their current values,
          learned or default, until it is turned back on or reset.
        </Callout>
      )}
      {error && <Callout>{error}</Callout>}

      <ul className="thresholds">
        {state.thresholds.map((t) => {
          const label = KEY_LABELS[t.key] ?? { title: t.key, desc: "" };
          return (
            <li key={t.key} className="card threshold">
              <div className="threshold-text">
                <div className="threshold-title">
                  <h2 className="card-title">{label.title}</h2>
                  {t.tuned ? <Badge tone="accent">Learned</Badge> : <Badge>Default</Badge>}
                </div>
                <p className="card-desc">{label.desc}</p>
                <code className="threshold-key">{t.key}</code>
              </div>
              <div className="threshold-value">
                <span className="threshold-num num">{t.value.toFixed(2)}</span>
                {t.tuned && (
                  <>
                    <span className="hint num">default {t.default.toFixed(2)}</span>
                    <Button
                      variant="ghost"
                      size="sm"
                      icon={RotateCcw}
                      onClick={() => reset(t.key)}
                      loading={busy === t.key}
                      disabled={busy !== null}
                      className="threshold-reset"
                      aria-label={`Reset ${label.title.toLowerCase()} to its default`}
                    >
                      Reset
                    </Button>
                  </>
                )}
              </div>
              <RangeTrack t={t} />
            </li>
          );
        })}
      </ul>

      <h2 className="section-label">
        <History aria-hidden className="section-icon" /> History
      </h2>
      {state.history.length === 0 ? (
        <p className="hint">No changes yet.</p>
      ) : (
        <ol className="timeline timeline-lg">
          {state.history.map((h, i) => (
            <li key={`${h.created_at}-${i}`}>
              <When iso={h.created_at} className="timeline-time" />
              <div>
                <p>
                  <strong>{KEY_LABELS[h.key]?.title ?? h.key}</strong>{" "}
                  <span className="change num">
                    {h.old_value?.toFixed(2) ?? "default"}
                    <ChevronRight aria-label="to" />
                    {h.new_value?.toFixed(2) ?? "default"}
                  </span>{" "}
                  <span className="muted">by {h.actor}</span>
                </p>
                {Object.keys(h.reason).length > 0 && (
                  <details className="history">
                    <summary>Why</summary>
                    <pre className="evidence">{JSON.stringify(h.reason, null, 2)}</pre>
                  </details>
                )}
              </div>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
