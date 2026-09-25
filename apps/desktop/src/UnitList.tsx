import { useEffect, useRef, useState, type ReactNode } from "react";
import { listUnits, type UnitSummary } from "./api";
import { Button, Callout, KindTag, Skeleton, errorText } from "./ui";

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
  /** Tighter rows, for side panels. */
  compact?: boolean;
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
  compact = false,
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
      .catch((e: unknown) => !cancelled && setError(errorText(e)));
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
      if (current.current === requested) setError(errorText(e));
    } finally {
      if (current.current === requested) setLoadingMore(false);
    }
  };

  if (error) return <Callout title="Couldn't load statements">{error}</Callout>;
  if (units === null) return <Skeleton rows={3} />;
  if (units.length === 0) return <>{empty}</>;
  return (
    <>
      <ul className={compact ? "units units-compact" : "units"}>
        {units.map((u) => (
          <li key={u.id} className="unit">
            <KindTag kind={u.kind} />
            <span className="unit-text">{u.statement}</span>
          </li>
        ))}
      </ul>
      {hasMore && (
        <Button variant="ghost" size="sm" onClick={loadMore} loading={loadingMore}>
          Show more
        </Button>
      )}
    </>
  );
}
