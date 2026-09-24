import { useCallback, useEffect, useState } from "react";
import { listClusters, type ClusterKind, type ClusterSummary } from "../api";

const PAGE_SIZE = 50;

export interface PagedClusters {
  items: ClusterSummary[];
  error: string | null;
  loading: boolean;
  /** True while the last page came back full, i.e. there may be more. */
  hasMore: boolean;
  loadMore: () => void;
}

/** Clusters of one kind, fetched a page at a time and appended. */
export function usePagedClusters(kind: ClusterKind): PagedClusters {
  const [items, setItems] = useState<ClusterSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [hasMore, setHasMore] = useState(false);

  const fetchPage = useCallback(
    async (offset: number, isCancelled: () => boolean) => {
      setLoading(true);
      try {
        const page = await listClusters(kind, PAGE_SIZE, offset);
        if (isCancelled()) return;
        setItems((prev) => (offset === 0 ? page : [...prev, ...page]));
        setHasMore(page.length === PAGE_SIZE);
        setError(null);
      } catch (e) {
        if (!isCancelled()) setError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!isCancelled()) setLoading(false);
      }
    },
    [kind],
  );

  useEffect(() => {
    let cancelled = false;
    setItems([]);
    fetchPage(0, () => cancelled);
    return () => {
      cancelled = true;
    };
  }, [fetchPage]);

  const loadMore = useCallback(() => {
    fetchPage(items.length, () => false);
  }, [fetchPage, items.length]);

  return { items, error, loading, hasMore, loadMore };
}
