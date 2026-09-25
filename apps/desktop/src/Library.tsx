import { useCallback, useEffect, useMemo, useState } from "react";
import {
  getArtifact,
  getArtifactContent,
  listArtifacts,
  listUnits,
  search,
  type ArtifactContent,
  type ArtifactDetail,
  type ArtifactSummary,
  type SearchHit,
  type UnitSummary,
} from "./api";
import Thumbnail from "./Thumbnail";

const PAGE = 50;
/** How often to refresh while a file is still being read. */
const POLL_MS = 10_000;

export const KIND_LABELS: Record<string, string> = {
  document_pdf: "PDF",
  document_markdown: "Markdown",
  document_text: "Text",
  image_photo: "Photo",
  image_screenshot: "Screenshot",
  chat_export: "Chat export",
  agent_log: "Agent log",
};

function fileName(a: Pick<ArtifactSummary, "original_filename" | "source_platform">): string {
  return a.original_filename ?? `(${a.source_platform} import)`;
}

function sizeLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function StatusBadge({ file }: { file: ArtifactSummary }) {
  if (file.status === "processing") return <span className="lib-badge reading">Reading…</span>;
  if (file.status === "failed") return <span className="lib-badge failed">Couldn't read</span>;
  return (
    <span className={file.unit_count > 0 ? "lib-badge found" : "lib-badge"}>
      {file.unit_count === 1 ? "1 item" : `${file.unit_count} items`}
    </span>
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
      {text.split(pattern).map((part, i) =>
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
  const body = text.slice(start, end).trim().replace(start > 0 ? /^\S*[\s.,;:!?]+/u : /^/, "");
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
      <p className="hint">
        Nothing matches "{results.query}". Search looks for the words themselves; try fewer or
        different words.
      </p>
    );
  }
  return (
    <div className="lib-search-results">
      {sections
        .filter(([, hits]) => hits.length > 0)
        .map(([title, hits]) => (
          <section key={title}>
            <h3>{title}</h3>
            <ul className="lib-hits">
              {hits.map((hit) => (
                <li key={`${hit.scope}-${hit.id}`}>
                  <button
                    className="lib-hit"
                    disabled={!hit.artifact_id}
                    onClick={() => hit.artifact_id && onOpen(hit.artifact_id)}
                  >
                    <span className="lib-hit-text">
                      <Highlight text={excerpt(hit.content, results.query)} query={results.query} />
                    </span>
                    {hit.artifact_id && (
                      <span className="lib-hit-source">
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
  const [units, setUnits] = useState<UnitSummary[] | null>(null);
  const [content, setContent] = useState<ArtifactContent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  // A different file starts from the top rather than showing the last one's text.
  useEffect(() => {
    setDetail(null);
    setUnits(null);
    setContent(null);
  }, [id]);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    Promise.all([getArtifact(id), listUnits({ artifactId: id }), getArtifactContent(id)])
      .then(([d, u, c]) => {
        if (cancelled) return;
        setDetail(d);
        setUnits(u);
        setContent(c);
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [id, refreshKey]);

  const loadMore = async () => {
    if (!content) return;
    setLoadingMore(true);
    try {
      const next = await getArtifactContent(id, 20, content.items.length);
      setContent({ ...next, items: [...content.items, ...next.items] });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoadingMore(false);
    }
  };

  if (error) return <p className="error">{error}</p>;
  if (!detail || !units || !content) return <p className="hint">Loading…</p>;

  return (
    <article className="lib-detail">
      <h2>{fileName(detail)}</h2>
      <p className="hint">
        {KIND_LABELS[detail.kind] ?? detail.kind} · {sizeLabel(detail.byte_size)} · added{" "}
        {new Date(detail.ingested_at).toLocaleString()}
        {detail.document?.page_count ? ` · ${detail.document.page_count} pages` : ""}
      </p>

      {detail.image && (
        <div className="lib-image">
          <Thumbnail imageId={detail.image.id} alt={fileName(detail)} size={240} />
          {detail.image.caption && <p>{detail.image.caption}</p>}
        </div>
      )}

      <section>
        <h3>What Gather found</h3>
        {detail.status === "processing" && (
          <p className="hint">
            Gather is still reading this file. Items appear here within a minute or two.
          </p>
        )}
        {detail.status === "failed" && (
          <p className="error">
            Gather couldn't read the text in this file. Details are in daemon.log.
          </p>
        )}
        {detail.status === "done" && units.length === 0 && (
          <p className="hint">
            Nothing in this file matched what Gather looks for yet. On its own, Gather picks up
            clear statements such as "I prefer…", "We decided to use…", "I work at…", "Our rent
            is $1,200" or "On 2026-03-01, …". With a local AI chat model (Ollama) it finds much
            more. The file's text is still stored and searchable.
          </p>
        )}
        {units.length > 0 && (
          <ul className="lib-units">
            {units.map((u) => (
              <li key={u.id}>
                <span className={`lib-kind kind-${u.kind}`}>{u.kind}</span>
                <span>{u.statement}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section>
        <h3>
          Contents
          {content.total > 0 && (
            <span className="hint">
              {" "}
              · {content.total} {content.source === "conversation" ? "messages" : "sections"}
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
          <div className="lib-passages">
            {content.items.map((p) => (
              <div className="lib-passage" key={p.seq}>
                {(p.heading || p.page || p.role) && (
                  <div className="lib-passage-head">
                    {[p.role, p.heading, p.page ? `page ${p.page}` : null]
                      .filter(Boolean)
                      .join(" · ")}
                  </div>
                )}
                <p>{p.text}</p>
              </div>
            ))}
            {content.items.length < content.total && (
              <button className="link-button" onClick={loadMore} disabled={loadingMore}>
                {loadingMore
                  ? "Loading…"
                  : `Show more (${content.total - content.items.length} left)`}
              </button>
            )}
          </div>
        )}
      </section>
    </article>
  );
}

interface LibraryProps {
  /** A file to open, e.g. from the graph. */
  focusId: string | null;
}

export default function Library({ focusId }: LibraryProps) {
  const [files, setFiles] = useState<ArtifactSummary[] | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(focusId);
  const [refreshKey, setRefreshKey] = useState(0);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchResults | null>(null);
  const [searching, setSearching] = useState(false);

  useEffect(() => {
    if (focusId) {
      setSelected(focusId);
      setResults(null);
    }
  }, [focusId]);

  const loadFiles = useCallback(async (count: number) => {
    try {
      const page = await listArtifacts(count + 1, 0);
      setHasMore(page.length > count);
      setFiles(page.slice(0, count));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
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

  const names = useMemo(
    () => new Map((files ?? []).map((f) => [f.id, fileName(f)])),
    [files],
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
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSearching(false);
    }
  };

  const open = (id: string) => {
    setSelected(id);
    setResults(null);
  };

  return (
    <div className="library">
      <form
        className="lib-search"
        onSubmit={(e) => {
          e.preventDefault();
          runSearch(query);
        }}
      >
        <input
          type="search"
          placeholder="Search everything you've added…"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            if (!e.target.value.trim()) setResults(null);
          }}
        />
        <button type="submit" disabled={searching || !query.trim()}>
          {searching ? "Searching…" : "Search"}
        </button>
      </form>

      {error && <p className="error">{error}</p>}

      {results ? (
        <>
          <button className="link-button" onClick={() => setResults(null)}>
            ← Back to your files
          </button>
          <SearchResultList results={results} names={names} onOpen={open} />
        </>
      ) : files === null ? (
        <p className="hint">Loading…</p>
      ) : files.length === 0 ? (
        <p className="hint">
          Nothing here yet. Add files on the Upload tab and they'll appear here, with what Gather
          found in each.
        </p>
      ) : (
        <div className="lib-panes">
          <ul className="lib-files">
            {files.map((f) => (
              <li key={f.id}>
                <button
                  className={f.id === selected ? "lib-file active" : "lib-file"}
                  onClick={() => setSelected(f.id)}
                >
                  <span className="lib-file-name">{fileName(f)}</span>
                  <span className="lib-file-meta">
                    {KIND_LABELS[f.kind] ?? f.kind} ·{" "}
                    {new Date(f.ingested_at).toLocaleDateString()}
                  </span>
                  <StatusBadge file={f} />
                </button>
              </li>
            ))}
            {hasMore && (
              <li>
                <button className="link-button" onClick={() => loadFiles(shown + PAGE)}>
                  Show more files
                </button>
              </li>
            )}
          </ul>
          <div className="lib-detail-pane">
            {selected ? (
              <FileDetail id={selected} refreshKey={refreshKey} />
            ) : (
              <p className="hint">
                Pick a file to see what Gather found in it and read its contents.
              </p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
