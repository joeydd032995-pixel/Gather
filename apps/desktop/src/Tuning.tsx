import { useState } from "react";
import { getTuning, resetTuning, type TuningKey } from "./api";
import { useAsync } from "./hooks/useAsync";

const KEY_LABELS: Record<TuningKey, string> = {
  "admit.hold_below": "Facts below this confidence go to the review tray",
  "merge.auto_single": "Duplicates at or above this similarity merge automatically",
};

/** What the pipeline learned from your answers, why, and how to undo it. */
export default function Tuning() {
  const tuning = useAsync(getTuning, []);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = async (key?: TuningKey) => {
    setBusy(true);
    setError(null);
    try {
      await resetTuning(key);
      tuning.reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (tuning.loading && !tuning.data) return <p>Loading…</p>;
  if (tuning.error) return <p className="error">{tuning.error}</p>;
  const state = tuning.data;
  if (!state) return null;

  return (
    <section>
      <p className="hint">
        {state.enabled
          ? `Gather adjusts these from your review answers: it tightens when fewer than ${Math.round(
              state.target_precision * 100,
            )}% of auto-accepted items hold up, and loosens only on strong evidence (at least ${
              state.min_samples
            } answers).`
          : "Auto-tuning is off (GATHER_TUNE_ENABLED=false); thresholds stay at their defaults."}
      </p>
      {error && <p className="error">{error}</p>}
      <table className="results">
        <thead>
          <tr>
            <th>Threshold</th>
            <th>Now</th>
            <th>Default</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {state.thresholds.map((t) => (
            <tr key={t.key}>
              <td title={t.key}>{KEY_LABELS[t.key] ?? t.key}</td>
              <td>
                {t.value.toFixed(2)}
                {t.tuned && <span className="prov-badge">learned</span>}
              </td>
              <td>{t.default.toFixed(2)}</td>
              <td>
                {t.tuned && (
                  <button onClick={() => reset(t.key)} disabled={busy}>
                    Reset
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <h3>History</h3>
      {state.history.length === 0 ? (
        <p className="prov-empty">No changes yet.</p>
      ) : (
        <ul className="prov-list">
          {state.history.map((h, i) => (
            <li key={`${h.created_at}-${i}`}>
              <span className="prov-time">{new Date(h.created_at).toLocaleString()}</span>{" "}
              {h.key}: {h.old_value?.toFixed(2) ?? "default"} →{" "}
              {h.new_value?.toFixed(2) ?? "default"}{" "}
              <span className="prov-kind">({h.actor})</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
