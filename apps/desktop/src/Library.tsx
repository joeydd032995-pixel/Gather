import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeft,
  CircleSlash,
  FileSearch,
  FolderOpen,
  LoaderCircle,
  Search,
  SearchX,
  Sparkles,
} from "lucide-react";
import {
  getArtifact,
  getArtifactContent,
  listArtifacts,
  search,
  type ArtifactContent,
  type ArtifactDetail,
  type ArtifactSummary,
  type SearchHit,
} from "./api";
import { kindIcon, kindLabel, plural, sizeLabel } from "./kinds";
import Thumbnail from "./Thumbnail";
import { Badge, Button, Callout, EmptyState, PageHeader, Skeleton, When, errorText } from "./ui";
import UnitList from "./UnitList";

const PAGE = 50;
/** How often to refresh while a file is still being read. */
const POLL_MS = 10_000;

function fileName(a: Pick<ArtifactSummary, "original_filename" | "source_platform">): string {
  return a.original_filename ?? `(${a.source_platform} import)`;
}

function StatusBadge({ file }: { file: ArtifactSummary }) {
  if (file.status === "processing") {
    return (
      <Badge tone="warning" icon={LoaderCircle} className="badge-reading">
        Reading
      </Badge>
    );
  }
  if (file.status === "failed") {
    return (
      <Badge tone="danger" icon={CircleSlash}>
        Unreadable
      </Badge>
    );
  }
  return (
    <Badge tone={file.unit_count > 0 ? "accent" : "neutral"}>
      {plural(file.unit_count, "item")}
    </Badge>
  );
}

/** `text` with the words of `query` wrapped in <mark>. */
function Highlight({ text, query }: { text: string; query: string }) {
  const words = query
    .split(/\s+/)
    .map((w) => w.replace(/[^\p{L}\p{N}]/gu, ""))
    .filter((w) => w.length > 1);
  if (words.length === 0) return <>{text}</>;
  const pattern = new RegExp(`(${words.join("|")})`, "giu");
  return (
    <>
      {text
        .split(pattern)
        .map((part, i) =>
          i % 2 === 1 ? <mark key={i}>{part}</mark> : <span key={i}>{part}</span>,
        )}
    </>
  );
}

/** A long passage shortened around the first query match. */
function excerpt(text: string, query: string, room = 240): string {
  if (text.length <= room) return text;
  const first = query.split(/\s+/).find((w) => w.length > 1);
  const at = first ? text.toLowerCase().indexOf(first.toLowerCase()) : -1;
  const start = Math.max(0, (at < 0 ? 0 : at) - room / 3);
  const end = Math.min(text.length, start + room);
  // Start the excerpt at a word, not mid-word or on stray punctuation.
  const body = text
    .slice(start, end)
    .trim()
    .replace(start > 0 ? /^\S*[\s.,;:!?]+/u : /^/, "");
  return `${start > 0 ? "…" : ""}${body}${end < text.length ? "…" : ""}`;
}

interface SearchResults {
  query: string;
  passages: SearchHit[];
  items: SearchHit[];
  chats: SearchHit[];
}

function SearchResultList({
  results,
  names,
  onOpen,
}: {
  results: SearchResults;
  names: Map<string, string>;
  onOpen: (id: string) => void;
}) {
  const sections: [string, SearchHit[]][] = [
    ["What Gather found", results.items],
    ["In your files", results.passages],
    ["In your chats", results.chats],
  ];
  const total = sections.reduce((n, [, hits]) => n + hits.length, 0);
  if (total === 0) {
    return (
      <EmptyState icon={SearchX} title={`Nothing matches “${results.query}”`}>
        Search looks for the words themselves. Try fewer or different words.
      </EmptyState>
    );
  }
  return (
    <div className="search-results" aria-live="polite">
      <p className="hint search-summary">
        {plural(total, "result")} for <strong>“{results.query}”</strong>
      </p>
      {sections
        .filter(([, hits]) => hits.length > 0)
        .map(([title, hits]) => (
          <section key={title}>
            <h2 className="section-label">
              {title} <span className="count">{hits.length}</span>
            </h2>
            <ul className="hits">
              {hits.map((hit) => (
                <li key={`${hit.scope}-${hit.id}`}>
                  <button
                    type="button"
                    className="hit"
                    disabled={!hit.artifact_id}
                    onClick={() => hit.artifact_id && onOpen(hit.artifact_id)}
                  >
                    <span className="hit-text">
                      <Highlight text={excerpt(hit.content, results.query)} query={results.query} />
                    </span>
                    {hit.artifact_id && (
                      <span className="hit-source">
                        <FolderOpen aria-hidden />
                        {names.get(hit.artifact_id) ?? "Open file"}
                      </span>
                    )}
                  </button>
                </li>
              ))}
            </ul>
          </section>
        ))}
    </div>
  );
}

function FileDetail({ id, refreshKey }: { id: string; refreshKey: number }) {
  const [detail, setDetail] = useState<ArtifactDetail | null>(null);
  const [content, setContent] = useState<ArtifactContent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  // The file whose content is on screen: a "Show more" page that arrives
  // after the user picked another file is dropped.
  const currentId = useRef(id);
  currentId.current = id;

  // A different file starts from the top rather than showing the last one's text.
  useEffect(() => {
    setDetail(null);
    setContent(null);
    setLoadingMore(false);
  }, [id]);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    Promise.all([getArtifact(id), getArtifactContent(id)])
      .then(([d, c]) => {
        if (cancelled) return;
        setDetail(d);
        setContent(c);
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(errorText(e));
      });
    return () => {
      cancelled = true;
    };
  }, [id, refreshKey]);

  const loadMore = async () => {
    if (!content) return;
    const requested = id;
    setLoadingMore(true);
    try {
      const next = await getArtifactContent(requested, 20, content.items.length);
      if (currentId.current !== requested) return;
      setContent({ ...next, items: [...content.items, ...next.items] });
    } catch (e) {
      if (currentId.current === requested) setError(errorText(e));
    } finally {
      if (currentId.current === requested) setLoadingMore(false);
    }
  };

  if (error) return <Callout title="Couldn't open this file">{error}</Callout>;
  if (!detail || !content) {
    return (
      <div className="doc">
        <Skeleton rows={5} />
      </div>
    );
  }

  const Icon = kindIcon(detail.kind);
  return (
    <article className="doc" aria-labelledby="doc-title">
      <header className="doc-head">
        <span className="file-icon file-icon-lg" aria-hidden>
          <Icon />
        </span>
        <div className="doc-heading">
          <h2 className="doc-title" id="doc-title">
            {fileName(detail)}
          </h2>
          <p className="doc-meta">
            <span>{kindLabel(detail.kind)}</span>
            <span className="dot-sep">{sizeLabel(detail.byte_size)}</span>
            {detail.document?.page_count ? (
              <span className="dot-sep">{plural(detail.document.page_count, "page")}</span>
            ) : null}
            <span className="dot-sep">
              Added <When iso={detail.ingested_at} />
            </span>
          </p>
        </div>
      </header>

      {detail.image && (
        <figure className="doc-image">
          <Thumbnail imageId={detail.image.id} alt={fileName(detail)} size={280} />
          {detail.image.caption && <figcaption>{detail.image.caption}</figcaption>}
        </figure>
      )}

      <section className="doc-section">
        <h3 className="section-label">
          <Sparkles aria-hidden className="section-icon" /> What Gather found
        </h3>
        {detail.status === "processing" && (
          <Callout tone="info" icon={LoaderCircle}>
            Gather is still reading this file. Items appear here within a minute or two.
          </Callout>
        )}
        {detail.status === "failed" && (
          <Callout title="Gather couldn't read the text in this file">
            Details are in daemon.log.
          </Callout>
        )}
        <UnitList
          artifactId={id}
          refreshKey={refreshKey}
          empty={
            detail.status === "done" && (
              <Callout tone="neutral" icon={FileSearch} title="Nothing extracted yet">
                On its own, Gather picks up clear statements such as “I prefer…”, “We decided to
                use…”, “I work at…”, “Our rent is $1,200” or “On 2026-03-01, …”. With a local AI
                chat model (Ollama) it finds much more. The file's text is still stored and
                searchable.
              </Callout>
            )
          }
        />
      </section>

      <section className="doc-section">
        <h3 className="section-label">
          Contents
          {content.total > 0 && (
            <span className="count">
              {plural(content.total, content.source === "conversation" ? "message" : "section")}
            </span>
          )}
        </h3>
        {content.items.length === 0 ? (
          <p className="hint">
            {content.source === "image"
              ? "No text was found in this image."
              : "No readable text was stored for this file."}
          </p>
        ) : (
          <div className={content.source === "conversation" ? "passages chat" : "passages"}>
            {content.items.map((p) => (
              <div className="passage" key={p.seq} data-role={p.role ?? undefined}>
                {(p.heading || p.page || p.role) && (
                  <div className="passage-head">
                    {p.role && <span className="passage-role">{p.role}</span>}
                    {p.heading && <span className="passage-heading">{p.heading}</span>}
                    {p.page && <span className="passage-page num">p. {p.page}</span>}
                  </div>
                )}
                <p>{p.text}</p>
              </div>
            ))}
            {content.items.length < content.total && (
              <Button variant="secondary" size="sm" onClick={loadMore} loading={loadingMore}>
                Show more · {content.total - content.items.length} left
              </Button>
            )}
          </div>
        )}
      </section>
    </article>
  );
}

interface LibraryProps {
  /** The open file. Kept by the parent so it survives switching tabs, and so
   * the graph and upload results can open a file here. */
  selected: string | null;
  onSelect: (id: string) => void;
}

export default function Library({ selected, onSelect }: LibraryProps) {
  const [files, setFiles] = useState<ArtifactSummary[] | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchResults | null>(null);
  const [searching, setSearching] = useState(false);
  const searchInput = useRef<HTMLInputElement>(null);

  const loadFiles = useCallback(async (count: number) => {
    try {
      const page = await listArtifacts(count + 1, 0);
      setHasMore(page.length > count);
      setFiles(page.slice(0, count));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  const shown = files?.length ?? 0;
  useEffect(() => {
    loadFiles(PAGE);
  }, [loadFiles]);

  // While anything is still being read, refresh so counts appear on their own.
  const reading = files?.some((f) => f.status === "processing") ?? false;
  useEffect(() => {
    if (!reading) return;
    const timer = setInterval(() => {
      loadFiles(Math.max(PAGE, shown));
      setRefreshKey((k) => k + 1);
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [reading, shown, loadFiles]);

  // "/" focuses search, as in most apps with one primary search box.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing =
        e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement;
      if (e.key === "/" && !typing && !e.metaKey && !e.ctrlKey) {
        e.preventDefault();
        searchInput.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const names = useMemo(() => new Map((files ?? []).map((f) => [f.id, fileName(f)])), [files]);

  const runSearch = async (text: string) => {
    const q = text.trim();
    if (!q) {
      setResults(null);
      return;
    }
    setSearching(true);
    try {
      const [items, passages, chats] = await Promise.all([
        search(q, "atomic_units"),
        search(q, "document_segments"),
        search(q, "messages"),
      ]);
      setResults({ query: q, items, passages, chats });
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setSearching(false);
    }
  };

  const open = (id: string) => {
    onSelect(id);
    setResults(null);
  };

  return (
    <div className="library">
      <PageHeader
        title="Library"
        description="Everything you've added, what Gather found in each file, and the text itself."
        eyebrow={
          files && files.length > 0
            ? `${plural(files.length, "file")}${hasMore ? "+" : ""}`
            : undefined
        }
      />

      <form
        className="library-search"
        role="search"
        onSubmit={(e) => {
          e.preventDefault();
          runSearch(query);
        }}
      >
        <div className="search-field search-field-lg">
          <Search aria-hidden />
          <input
            ref={searchInput}
            className="input"
            type="search"
            aria-label="Search your library"
            placeholder="Search everything you've added…"
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              if (!e.target.value.trim()) setResults(null);
            }}
          />
          {!query && (
            <span className="search-hint" aria-hidden>
              Press <kbd className="kbd">/</kbd>
            </span>
          )}
        </div>
        <Button
          type="submit"
          variant="primary"
          size="lg"
          loading={searching}
          disabled={!query.trim()}
        >
          Search
        </Button>
      </form>

      {error && <Callout title="Something went wrong">{error}</Callout>}

      {results ? (
        <>
          <Button
            variant="ghost"
            size="sm"
            icon={ArrowLeft}
            onClick={() => setResults(null)}
            className="back"
          >
            Back to your files
          </Button>
          <SearchResultList results={results} names={names} onOpen={open} />
        </>
      ) : files === null && error ? null : files === null ? (
        <div className="library-panes">
          <div className="file-list card">
            <Skeleton rows={7} />
          </div>
          <div />
        </div>
      ) : files.length === 0 ? (
        <EmptyState icon={FolderOpen} title="Your library is empty">
          Add files from <strong>Add files</strong> and they'll appear here, with what Gather found
          in each.
        </EmptyState>
      ) : (
        <div className="library-panes">
          <nav className="file-list card" aria-label="Files">
            <ul>
              {files.map((f) => {
                const Icon = kindIcon(f.kind);
                const active = f.id === selected;
                return (
                  <li key={f.id}>
                    <button
                      type="button"
                      data-file
                      className={active ? "file-row active" : "file-row"}
                      aria-current={active ? "true" : undefined}
                      onClick={() => onSelect(f.id)}
                    >
                      <span className="file-icon" aria-hidden>
                        <Icon />
                      </span>
                      <span className="file-text">
                        <span className="file-name">{fileName(f)}</span>
                        <span className="file-meta">
                          {kindLabel(f.kind)}
                          <span className="dot-sep">
                            <When iso={f.ingested_at} />
                          </span>
                        </span>
                      </span>
                      <StatusBadge file={f} />
                    </button>
                  </li>
                );
              })}
            </ul>
            {hasMore && (
              <div className="file-list-more">
                <Button variant="ghost" size="sm" onClick={() => loadFiles(shown + PAGE)}>
                  Show more files
                </Button>
              </div>
            )}
          </nav>
          <div className="library-detail">
            {selected ? (
              <FileDetail id={selected} refreshKey={refreshKey} />
            ) : (
              <EmptyState icon={FileSearch} title="Pick a file">
                See what Gather found in it, and read its contents.
              </EmptyState>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
