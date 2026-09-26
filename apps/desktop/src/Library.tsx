import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CircleSlash,
  FileSearch,
  FolderOpen,
  Library as LibraryIcon,
  LoaderCircle,
  Plus,
  Search,
  SearchX,
  Sparkles,
  TextQuote,
  X,
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
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  IconButton,
  Kbd,
  Panel,
  Segmented,
  Skeleton,
  SplitView,
  Toolbar,
  When,
  errorText,
} from "./ui";
import UnitList from "./UnitList";

const PAGE = 50;
/** How often to refresh while a file is still being read. */
const POLL_MS = 10_000;

type KindFilter = "all" | "documents" | "chats" | "images";
const FILTERS: { value: KindFilter; label: string }[] = [
  { value: "all", label: "All" },
  { value: "documents", label: "Docs" },
  { value: "chats", label: "Chats" },
  { value: "images", label: "Images" },
];

function matchesFilter(kind: string, filter: KindFilter): boolean {
  if (filter === "all") return true;
  if (filter === "documents") return kind.startsWith("document");
  if (filter === "images") return kind.startsWith("image");
  return kind === "chat_export" || kind === "agent_log";
}

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
function excerpt(text: string, query: string, room = 180): string {
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
  selected,
  onOpen,
}: {
  results: SearchResults;
  names: Map<string, string>;
  selected: string | null;
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
      <EmptyState icon={SearchX} title={`Nothing matches “${results.query}”`} compact>
        Search looks for the words themselves. Try fewer or different words.
      </EmptyState>
    );
  }
  return (
    <div>
      {sections
        .filter(([, hits]) => hits.length > 0)
        .map(([title, hits]) => (
          <section key={title}>
            <h3 className="list-group-label">
              {title} · <span className="num">{hits.length}</span>
            </h3>
            <ul className="rows">
              {hits.map((hit) => (
                <li key={`${hit.scope}-${hit.id}`}>
                  <button
                    type="button"
                    className="row"
                    disabled={!hit.artifact_id}
                    aria-current={hit.artifact_id === selected ? "true" : undefined}
                    onClick={() => hit.artifact_id && onOpen(hit.artifact_id)}
                  >
                    <span className="row-lead" aria-hidden>
                      <TextQuote />
                    </span>
                    <span className="row-main">
                      <span className="row-title wrap hit-text">
                        <Highlight
                          text={excerpt(hit.content, results.query)}
                          query={results.query}
                        />
                      </span>
                      {hit.artifact_id && (
                        <span className="row-meta">
                          <FolderOpen aria-hidden className="meta-icon" />
                          {names.get(hit.artifact_id) ?? "Open file"}
                        </span>
                      )}
                    </span>
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

  if (error) {
    return (
      <div className="inspector">
        <Callout title="Couldn't open this file">{error}</Callout>
      </div>
    );
  }
  if (!detail || !content) {
    return (
      <div className="inspector">
        <Skeleton rows={5} />
      </div>
    );
  }

  const Icon = kindIcon(detail.kind);
  const family = detail.kind.split("_")[0];
  return (
    <article className="inspector" aria-labelledby="doc-title" key={id}>
      <header className="inspector-head">
        <span className="file-tile" data-family={family} aria-hidden>
          <Icon />
        </span>
        <div className="min0">
          <h2 className="inspector-title" id="doc-title">
            {fileName(detail)}
          </h2>
          <p className="inspector-sub">
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
          <Thumbnail imageId={detail.image.id} alt={fileName(detail)} size={320} />
          {detail.image.caption && <figcaption>{detail.image.caption}</figcaption>}
        </figure>
      )}

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

      <Panel title="What Gather found" icon={Sparkles} className="doc-panel">
        <UnitList
          artifactId={id}
          refreshKey={refreshKey}
          empty={
            detail.status === "done" && (
              <div className="panel-pad">
                <Callout tone="neutral" icon={FileSearch} title="Nothing extracted yet">
                  On its own, Gather picks up clear statements such as “I prefer…”, “We decided to
                  use…”, “I work at…”, “Our rent is $1,200” or “On 2026-03-01, …”. With a local AI
                  chat model (Ollama) it finds much more. The file's text is still stored and
                  searchable.
                </Callout>
              </div>
            )
          }
        />
      </Panel>

      <Panel
        title="Contents"
        icon={TextQuote}
        className="doc-panel"
        actions={
          content.total > 0 && (
            <span className="hint num">
              {plural(content.total, content.source === "conversation" ? "message" : "section")}
            </span>
          )
        }
      >
        {content.items.length === 0 ? (
          <p className="panel-pad hint">
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
      </Panel>
    </article>
  );
}

interface LibraryProps {
  /** The open file. Kept by the parent so it survives switching views, and
   * so the graph and uploads can open a file here. */
  selected: string | null;
  onSelect: (id: string) => void;
  onAddFiles: () => void;
}

export default function Library({ selected, onSelect, onAddFiles }: LibraryProps) {
  const [files, setFiles] = useState<ArtifactSummary[] | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchResults | null>(null);
  const [searching, setSearching] = useState(false);
  const [filter, setFilter] = useState<KindFilter>("all");
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

  // Open the newest file when nothing is selected yet, so the right-hand
  // pane is never an empty frame.
  useEffect(() => {
    if (!selected && files && files.length > 0) onSelect(files[0].id);
  }, [files, selected, onSelect]);

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
  const visible = useMemo(
    () => (files ?? []).filter((f) => matchesFilter(f.kind, filter)),
    [files, filter],
  );

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

  const clearSearch = () => {
    setQuery("");
    setResults(null);
  };

  const toolbar = (
    <Toolbar
      title="Library"
      icon={LibraryIcon}
      count={files ? `${shown}${hasMore ? "+" : ""}` : undefined}
    >
      <form
        role="search"
        className="toolbar-search"
        onSubmit={(e) => {
          e.preventDefault();
          runSearch(query);
        }}
      >
        <div className="search">
          {searching ? <LoaderCircle className="spin" aria-hidden /> : <Search aria-hidden />}
          <input
            ref={searchInput}
            className="input"
            type="search"
            aria-label="Search your library"
            placeholder="Search everything…"
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              if (!e.target.value.trim()) setResults(null);
            }}
            onKeyDown={(e) => e.key === "Escape" && clearSearch()}
          />
          {!query && <Kbd>/</Kbd>}
        </div>
      </form>
      <span className="toolbar-sep" aria-hidden />
      <Button variant="primary" size="sm" icon={Plus} onClick={onAddFiles}>
        Add
      </Button>
    </Toolbar>
  );

  if (files !== null && files.length === 0 && !results) {
    return (
      <>
        {toolbar}
        <EmptyState
          icon={FolderOpen}
          title="Your library is empty"
          action={
            <Button variant="primary" icon={Plus} onClick={onAddFiles}>
              Add files…
            </Button>
          }
        >
          Drop files anywhere in this window, or choose them. What Gather finds in each appears
          here.
        </EmptyState>
      </>
    );
  }

  const list = results ? (
    <SearchResultList results={results} names={names} selected={selected} onOpen={onSelect} />
  ) : files === null ? (
    error ? null : (
      <Skeleton rows={8} />
    )
  ) : (
    <>
      <ul className="rows">
        {visible.map((f) => {
          const Icon = kindIcon(f.kind);
          const active = f.id === selected;
          return (
            <li key={f.id}>
              <button
                type="button"
                data-file
                className="row"
                aria-current={active ? "true" : undefined}
                onClick={() => onSelect(f.id)}
              >
                <span className="row-lead" data-family={f.kind.split("_")[0]} aria-hidden>
                  <Icon />
                </span>
                <span className="row-main">
                  <span className="row-title">{fileName(f)}</span>
                  <span className="row-meta">
                    {kindLabel(f.kind)}
                    <span className="dot-sep">
                      <When iso={f.ingested_at} />
                    </span>
                  </span>
                </span>
                <span className="row-trail">
                  <StatusBadge file={f} />
                </span>
              </button>
            </li>
          );
        })}
      </ul>
      {visible.length === 0 && (
        <p className="list-empty hint">
          No {FILTERS.find((x) => x.value === filter)?.label.toLowerCase()} yet.
        </p>
      )}
      {hasMore && (
        <div className="list-more">
          <Button variant="ghost" size="sm" onClick={() => loadFiles(shown + PAGE)}>
            Show more files
          </Button>
        </div>
      )}
    </>
  );

  return (
    <>
      {toolbar}
      {error && (
        <div className="view-callout">
          <Callout title="Something went wrong">{error}</Callout>
        </div>
      )}
      <SplitView
        listLabel={results ? "Search results" : "Files"}
        listHeader={
          results ? (
            <div className="list-head-row">
              <span className="hint">
                Results for <strong>“{results.query}”</strong>
              </span>
              <IconButton icon={X} label="Clear search" size="sm" onClick={clearSearch} />
            </div>
          ) : (
            <Segmented label="Show" options={FILTERS} value={filter} onChange={setFilter} />
          )
        }
        list={list}
        detail={
          selected ? (
            <FileDetail id={selected} refreshKey={refreshKey} />
          ) : (
            <EmptyState icon={FileSearch} title="Pick a file">
              See what Gather found in it, and read its contents.
            </EmptyState>
          )
        }
      />
    </>
  );
}
