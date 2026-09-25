import { useCallback, useEffect, useState } from "react";
import { errorText } from "../ui";

export interface AsyncState<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  /** Re-run the loader (e.g. after an action changed the server state). */
  reload: () => void;
}

/**
 * Run an async loader on mount and whenever `deps` change, ignoring results
 * that arrive after the component moved on (unmount or a newer request).
 */
export function useAsync<T>(loader: () => Promise<T>, deps: unknown[]): AsyncState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [generation, setGeneration] = useState(0);

  // The caller's deps decide when the loader is "new", like useEffect's.
  const load = useCallback(loader, deps);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    load()
      .then((value) => {
        if (cancelled) return;
        setData(value);
        setError(null);
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setError(errorText(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [load, generation]);

  const reload = useCallback(() => setGeneration((g) => g + 1), []);
  return { data, error, loading, reload };
}
