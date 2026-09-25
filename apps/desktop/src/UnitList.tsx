import { useEffect, useRef, useState, type ReactNode } from "react";
import { listUnits, type UnitSummary } from "./api";

interface UnitListProps {
  /** Units extracted from this file… */
  artifactId?: string;
  /** …or units about this entity. */
  subjectEntityId?: string;
  pageSize?: number;
  /** Bump to reload from the first page. */
  refreshKey?: number;
  /** Shown when there are no units. */
  empty?: ReactNode;
}

/**
 * Statements Gather currently holds (retracted or superseded ones are left
 * out), a page at a time.
 */
export default function UnitList({
  artifactId,
  subjectEntityId,
  pageSize = 100,
  refreshKey = 0,
  empty = null,
}: UnitListProps) {
  const [units, setUnits] = useState<UnitSummary[] | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The list a response belongs to: a page that arrives after the user moved
  // on to another file or entity is dropped.
  const key = `${artifactId ?? ""}|${subjectEntityId ?? ""}`;
  const current = useRef(key);
  current.current = key;

  useEffect(() => {
    let cancelled = false;
    setUnits(null);
    setError(null);
    listUnits({ artifactId, subjectEntityId, limit: pageSize, offset: 0 })
      .then((page) => {
        if (cancelled) return;
        setUnits(page);
        setHasMore(page.length === pageSize);
      })
      .catch((e: unknown) => !cancelled && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      cancelled = true;
    };
  }, [artifactId, subjectEntityId, pageSize, refreshKey]);

  const loadMore = async () => {
    if (!units) return;
    const requested = key;
    setLoadingMore(true);
    try {
      const page = await listUnits({
        artifactId,
        subjectEntityId,
        limit: pageSize,
        offset: units.length,
      });
      if (current.current !== requested) return;
      setUnits([...units, ...page]);
      setHasMore(page.length === pageSize);
    } catch (e) {
      if (current.current === requested) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (current.current === requested) setLoadingMore(false);
    }
  };

  if (error) return <p className="error">{error}</p>;
  if (units === null) return <p className="hint">Loading…</p>;
  if (units.length === 0) return <>{empty}</>;
  return (
    <>
      <ul className="lib-units">
        {units.map((u) => (
          <li key={u.id}>
            <span className={`lib-kind kind-${u.kind}`}>{u.kind}</span>
            <span>{u.statement}</span>
          </li>
        ))}
      </ul>
      {hasMore && (
        <button className="link-button" onClick={loadMore} disabled={loadingMore}>
          {loadingMore ? "Loading…" : "Show more"}
        </button>
      )}
    </>
  );
}
