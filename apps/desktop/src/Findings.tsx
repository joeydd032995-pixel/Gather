import { useEffect, useRef, useState } from "react";
import {
  FileText,
  LoaderCircle,
  Search,
  SearchX,
  Sparkles,
} from "lucide-react";
import { listFindings, type UnitSummary } from "./api";
import { plural } from "./kinds";
import {
  Button,
  Callout,
  EmptyState,
  KindTag,
  Segmented,
  Skeleton,
  Toolbar,
  When,
  errorText,
} from "./ui";

const PAGE = 100;

const KINDS = [
  { value: "all", label: "All" },
  { value: "fact", label: "Facts" },
  { value: "claim", label: "Claims" },
  { value: "decision", label: "Decisions" },
  { value: "preference", label: "Preferences" },
  { value: "event", label: "Events" },
] as const;

type KindFilter = (typeof KINDS)[number]["value"];

interface FindingsProps {
  /** Open the file a statement came from in the Library. */
  onOpenFile: (id: string) => void;
  /** Bump to reload (a new upload may have added statements). */
  refreshKey?: number;
}

/**
 * Every statement Gather currently holds, across all files, newest first: the
 * same list as "What Gather found" on a file's page, but for everything, with
 * a text filter, a kind filter and the file each one came from.
 */
export default function Findings({
  onOpenFile,
  refreshKey = 0,
}: FindingsProps) {
  const [kind, setKind] = useState<KindFilter>("all");
  const [query, setQuery] = useState("");
  const [text, setText] = useState("");
  const [items, setItems] = useState<UnitSummary[] | null>(null);
  const [total, setTotal] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const searchInput = useRef<HTMLInputElement>(null);

  // Typing waits a moment before it searches.
  useEffect(() => {
    const t = setTimeout(() => setText(query.trim()), 250);
    return () => clearTimeout(t);
  }, [query]);

  // The filter a response belongs to: a page for an earlier filter is dropped.
  const key = `${kind}|${text}|${refreshKey}`;
  const current = useRef(key);
  current.current = key;

  useEffect(() => {
    let cancelled = false;
    setItems(null);
    setError(null);
    listFindings({
      kind: kind === "all" ? undefined : kind,
      q: text,
      limit: PAGE,
      offset: 0,
    })
      .then((page) => {
        if (cancelled) return;
        setItems(page.items);
        setTotal(page.total);
      })
      .catch((e: unknown) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [kind, text, refreshKey]);

  const loadMore = async () => {
    if (!items) return;
    const requested = key;
    setLoadingMore(true);
    try {
      const page = await listFindings({
        kind: kind === "all" ? undefined : kind,
        q: text,
        limit: PAGE,
        offset: items.length,
      });
      if (current.current !== requested) return;
      setItems([...items, ...page.items]);
      setTotal(page.total);
    } catch (e) {
      if (current.current === requested) setError(errorText(e));
    } finally {
      if (current.current === requested) setLoadingMore(false);
    }
  };

  const filtered = kind !== "all" || text !== "";
  const hasMore = items !== null && items.length < total;

  return (
    <>
      <Toolbar
        title="What Gather found"
        icon={Sparkles}
        count={items ? total : undefined}
      >
        <div className="search">
          {text !== query.trim() ? (
            <LoaderCircle className="spin" aria-hidden />
          ) : (
            <Search aria-hidden />
          )}
          <input
            ref={searchInput}
            className="input"
            type="search"
            aria-label="Filter statements"
            placeholder="Filter statements…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => e.key === "Escape" && setQuery("")}
          />
        </div>
        <Segmented
          label="Kind of statement"
          options={[...KINDS]}
          value={kind}
          onChange={setKind}
        />
      </Toolbar>
      {error && (
        <div className="view-callout">
          <Callout title="Couldn't load what Gather found">{error}</Callout>
        </div>
      )}
      {items === null && !error && <Skeleton rows={6} />}
      {items !== null && items.length === 0 && (
        <EmptyState
          icon={filtered ? SearchX : Sparkles}
          title={filtered ? "No statements match" : "Nothing learned yet"}
        >
          {filtered
            ? "Try other words, or look at all kinds."
            : "Statements appear here as Gather reads your files. Add some from the Library."}
        </EmptyState>
      )}
      {items !== null && items.length > 0 && (
        <div className="findings">
          <p className="hint findings-count">
            Showing {items.length} of {plural(total, "statement")}
            {filtered ? " matching" : ""}, newest first.
          </p>
          <ul className="units findings-list">
            {items.map((u) => (
              <li key={u.id} className="unit finding">
                <KindTag kind={u.kind} />
                <div className="finding-body">
                  <span className="unit-text">{u.statement}</span>
                  <span className="finding-meta">
                    {u.source_artifact_id ? (
                      <button
                        type="button"
                        className="finding-source"
                        onClick={() => onOpenFile(u.source_artifact_id!)}
                        title="Open this file in the Library"
                      >
                        <FileText aria-hidden />
                        {u.source_name || "Untitled file"}
                      </button>
                    ) : (
                      <span className="finding-source none">No file</span>
                    )}
                    {u.created_at && (
                      <When iso={u.created_at} className="hint" />
                    )}
                  </span>
                </div>
              </li>
            ))}
          </ul>
          {hasMore && (
            <div className="findings-more">
              <Button
                variant="ghost"
                size="sm"
                onClick={loadMore}
                loading={loadingMore}
              >
                Show more
              </Button>
            </div>
          )}
        </div>
      )}
    </>
  );
}
