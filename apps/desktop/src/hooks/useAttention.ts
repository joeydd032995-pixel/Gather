import { useEffect, useState } from "react";
import { listContradictions, listMergeSuggestions, listReview } from "../api";
import type { Tab } from "../nav";

const POLL_MS = 20_000;

/** How many items wait in each "Needs you" view, for the sidebar badges. */
export function useAttention(enabled: boolean, tab: Tab): Partial<Record<Tab, number>> {
  const [counts, setCounts] = useState<Partial<Record<Tab, number>>>({});

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const load = async () => {
      const [review, contradictions, entities] = await Promise.allSettled([
        listReview(100),
        listContradictions("open"),
        listMergeSuggestions(),
      ]);
      if (cancelled) return;
      const size = (r: PromiseSettledResult<unknown[]>) =>
        r.status === "fulfilled" ? r.value.length : undefined;
      setCounts({
        review: size(review),
        contradictions: size(contradictions),
        entities: size(entities),
      });
    };
    load();
    const timer = setInterval(load, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
    // Re-count on navigation: acting in a view usually changes its count.
  }, [enabled, tab]);

  return counts;
}
