// Projects: folders (or .zip files) added as a whole, shown as the tree they
// came in — project, folders, files — with what Gather made of each file.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Archive,
  ArrowUpRight,
  ChevronRight,
  CircleSlash,
  Copy,
  FileArchive,
  Folder,
  FolderArchive,
  FolderMinus,
  FolderOpen,
  FolderPlus,
  FolderTree,
  Sparkles,
  Trash2,
  Waypoints,
} from "lucide-react";
import {
  deleteProject,
  getProject,
  getSimilarProjects,
  listProjects,
  type SimilarProject,
  type ProjectDetail,
  type ProjectItem,
  type ProjectSummary,
} from "./api";
import { useListKeys } from "./hooks/useListKeys";
import { kindIcon, kindLabel, plural, sizeLabel } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Panel,
  Segmented,
  Skeleton,
  SplitView,
  Toolbar,
  When,
  errorText,
} from "./ui";

type Filter = "all" | "skipped";

const keyOf = (p: ProjectSummary) => p.id;

function summaryLine(p: ProjectSummary): string {
  const parts = [plural(p.files, "file")];
  if (p.folders > 0) parts.push(plural(p.folders, "folder"));
  if (p.skipped + p.failed > 0) parts.push(`${p.skipped + p.failed} skipped`);
  if (p.left_out > 0) parts.push(`${plural(p.left_out, "folder")} left out`);
  return parts.join(" · ");
}

/** A folder left out whole (version control, dependencies, caches). */
const isLeftOut = (item: ProjectItem) => item.item_kind === "folder" && item.status === "skipped";
/** A .zip unpacked where it sat is a folder named after it. */
const isUnpackedZip = (item: ProjectItem) =>
  item.item_kind === "folder" && item.name.toLowerCase().endsWith(".zip");

const KEPT_HINT = "Kept as it is. Gather can't read text from this kind of file.";

/** Children by parent (null = the project root), folders first, then files. */
function childrenOf(items: ProjectItem[]): Map<string | null, ProjectItem[]> {
  const map = new Map<string | null, ProjectItem[]>();
  for (const item of items) {
    const list = map.get(item.parent_id) ?? [];
    list.push(item);
    map.set(item.parent_id, list);
  }
  for (const list of map.values()) {
    list.sort((a, b) =>
      a.item_kind !== b.item_kind
        ? a.item_kind === "folder"
          ? -1
          : 1
        : a.name.localeCompare(b.name, undefined, { numeric: true }),
    );
  }
  return map;
}

/** Files under each folder, at any depth. */
function fileCounts(children: Map<string | null, ProjectItem[]>): Map<string, number> {
  const counts = new Map<string, number>();
  const count = (id: string): number => {
    let n = 0;
    for (const child of children.get(id) ?? []) {
      n += child.item_kind === "file" ? 1 : count(child.id);
    }
    counts.set(id, n);
    return n;
  };
  for (const root of children.get(null) ?? []) if (root.item_kind === "folder") count(root.id);
  return counts;
}

function FileStatus({ item }: { item: ProjectItem }) {
  switch (item.status) {
    case "ingested":
      return (
        <span className="tree-status">
          {item.units > 0 ? plural(item.units, "statement") : "Read"}
        </span>
      );
    case "deduplicated":
      return (
        <span className="tree-status tone-warning" title="The same content is already in Gather">
          <Copy aria-hidden /> Already in Gather
        </span>
      );
    case "stored":
      return (
        <span className="tree-status" title={item.detail ?? KEPT_HINT}>
          <Archive aria-hidden /> Kept as is
        </span>
      );
    default:
      return (
        <span className={`tree-status tone-${item.status === "failed" ? "danger" : "neutral"}`}>
          <CircleSlash aria-hidden /> {item.detail ?? "Skipped"}
        </span>
      );
  }
}

function FileRow({
  item,
  onOpenFile,
}: {
  item: ProjectItem;
  onOpenFile: (artifactId: string) => void;
}) {
  const Icon = kindIcon(item.artifact_kind);
  const open = item.artifact_id;
  const body = (
    <>
      <span className="tree-icon" aria-hidden>
        <Icon />
      </span>
      <span className="tree-name">{item.name}</span>
      {item.artifact_kind && <span className="tree-kind">{kindLabel(item.artifact_kind)}</span>}
      <FileStatus item={item} />
      {open && <ArrowUpRight className="tree-go" aria-hidden />}
    </>
  );
  return open ? (
    <button
      type="button"
      className="tree-row tree-file"
      title={`Open ${item.path} in the Library`}
      onClick={() => onOpenFile(open)}
    >
      {body}
    </button>
  ) : (
    <div className={`tree-row tree-file ${item.status}`}>{body}</div>
  );
}

function Tree({
  parent,
  children,
  counts,
  expanded,
  onToggle,
  onOpenFile,
}: {
  parent: string | null;
  children: Map<string | null, ProjectItem[]>;
  counts: Map<string, number>;
  expanded: Set<string>;
  onToggle: (id: string) => void;
  onOpenFile: (artifactId: string) => void;
}) {
  const items = children.get(parent) ?? [];
  if (items.length === 0) return null;
  return (
    <ul className="tree" role={parent === null ? "tree" : "group"}>
      {items.map((item) =>
        isLeftOut(item) ? (
          <li key={item.id} role="treeitem">
            <div className="tree-row tree-folder skipped">
              <span className="tree-chevron-space" aria-hidden />
              <span className="tree-icon" aria-hidden>
                <FolderMinus />
              </span>
              <span className="tree-name">{item.name}</span>
              <span className="tree-status tone-neutral">
                <CircleSlash aria-hidden /> Left out: {item.detail}
              </span>
            </div>
          </li>
        ) : item.item_kind === "folder" ? (
          <li key={item.id} role="treeitem" aria-expanded={expanded.has(item.id)}>
            <button
              type="button"
              className="tree-row tree-folder"
              onClick={() => onToggle(item.id)}
            >
              <ChevronRight
                className={expanded.has(item.id) ? "tree-chevron open" : "tree-chevron"}
                aria-hidden
              />
              <span className="tree-icon" aria-hidden>
                {isUnpackedZip(item) ? (
                  <FolderArchive />
                ) : expanded.has(item.id) ? (
                  <FolderOpen />
                ) : (
                  <Folder />
                )}
              </span>
              <span className="tree-name">{item.name}</span>
              {isUnpackedZip(item) && <span className="tree-kind">Unpacked .zip</span>}
              <span className="tree-status">{plural(counts.get(item.id) ?? 0, "file")}</span>
            </button>
            {expanded.has(item.id) && (
              <Tree
                parent={item.id}
                children={children}
                counts={counts}
                expanded={expanded}
                onToggle={onToggle}
                onOpenFile={onOpenFile}
              />
            )}
          </li>
        ) : (
          <li key={item.id} role="treeitem">
            <FileRow item={item} onOpenFile={onOpenFile} />
          </li>
        ),
      )}
    </ul>
  );
}

/** The projects most like this one, with why. */
function SimilarPanel({
  id,
  refreshKey,
  onSelect,
}: {
  id: string;
  refreshKey: number;
  onSelect: (id: string) => void;
}) {
  const [items, setItems] = useState<SimilarProject[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // A new project shows at once; while files are still arriving, it waits
  // for a pause rather than comparing again after every file.
  const shownFor = useRef<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    const fresh = shownFor.current !== id;
    if (fresh) setItems(null);
    const timer = setTimeout(
      () => {
        getSimilarProjects(id, 8)
          .then((list) => {
            if (cancelled) return;
            shownFor.current = id;
            setItems(list);
            setError(null);
          })
          .catch((e) => !cancelled && setError(errorText(e)));
      },
      fresh ? 0 : 1500,
    );
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [id, refreshKey]);

  return (
    <Panel
      title="Similar projects"
      icon={Sparkles}
      actions={items && items.length > 0 && <span className="hint num">{items.length}</span>}
    >
      {error ? (
        <p className="panel-pad hint">{error}</p>
      ) : !items ? (
        <div className="panel-pad">
          <Skeleton rows={2} />
        </div>
      ) : items.length === 0 ? (
        <p className="panel-pad hint">
          No other project is much like this one yet. Gather compares the files they share, how
          their folders are laid out, the people and things they mention, and what their text is
          about.
        </p>
      ) : (
        <ul className="similar-list">
          {items.map((p) => (
            <li key={p.project_id}>
              <button
                type="button"
                className="similar-row"
                onClick={() => onSelect(p.project_id)}
                title={`Open ${p.name}`}
              >
                <span className="similar-head">
                  <FolderTree className="similar-icon" aria-hidden />
                  <span className="similar-name">{p.name}</span>
                  <span className="similar-score num">{Math.round(p.score * 100)}%</span>
                </span>
                <span className="similar-bar" aria-hidden>
                  <span style={{ width: `${Math.round(p.score * 100)}%` }} />
                </span>
                {p.reasons.length > 0 && (
                  <ul className="similar-reasons">
                    {p.reasons.map((r) => (
                      <li key={r}>{r}</li>
                    ))}
                  </ul>
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function ProjectView({
  id,
  refreshKey,
  onOpenFile,
  onRemoved,
  onSelectProject,
  onShowGraph,
}: {
  id: string;
  refreshKey: number;
  onOpenFile: (artifactId: string) => void;
  onRemoved: () => void;
  onSelectProject: (id: string) => void;
  onShowGraph?: (id: string) => void;
}) {
  const [detail, setDetail] = useState<ProjectDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [expanded, setExpanded] = useState<Set<string> | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [removing, setRemoving] = useState(false);

  useEffect(() => {
    setDetail(null);
    setExpanded(null);
    setFilter("all");
    setConfirming(false);
  }, [id]);

  useEffect(() => {
    let cancelled = false;
    getProject(id)
      .then((d) => {
        if (cancelled) return;
        setDetail(d);
        setError(null);
        // Top-level folders start open; the rest stay as the user left them.
        setExpanded(
          (prev) =>
            prev ??
            new Set(
              d.items.filter((i) => i.item_kind === "folder" && i.depth === 0).map((i) => i.id),
            ),
        );
      })
      .catch((e) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [id, refreshKey]);

  const children = useMemo(() => childrenOf(detail?.items ?? []), [detail]);
  const counts = useMemo(() => fileCounts(children), [children]);
  const skipped = useMemo(
    () => (detail?.items ?? []).filter((i) => i.status === "skipped" || i.status === "failed"),
    [detail],
  );
  const toggle = useCallback(
    (itemId: string) =>
      setExpanded((prev) => {
        const next = new Set(prev ?? []);
        if (next.has(itemId)) next.delete(itemId);
        else next.add(itemId);
        return next;
      }),
    [],
  );

  const remove = async () => {
    setRemoving(true);
    try {
      await deleteProject(id);
      onRemoved();
    } catch (e) {
      setError(errorText(e));
      setRemoving(false);
    }
  };

  if (!detail) {
    return (
      <div className="inspector">{error ? <Callout>{error}</Callout> : <Skeleton rows={6} />}</div>
    );
  }

  return (
    <div className="inspector" key={id}>
      <div className="inspector-kicker">
        <Badge tone="accent" icon={detail.source === "zip" ? FileArchive : FolderTree}>
          {detail.source === "zip" ? "From a .zip" : "Folder"}
        </Badge>
        <span className="hint">
          updated <When iso={detail.updated_at} />
        </span>
      </div>
      <h2 className="inspector-title">{detail.name}</h2>
      <p className="inspector-sub">
        {plural(detail.files, "file")}
        <span className="dot-sep">{detail.ingested} read</span>
        {detail.deduplicated > 0 && (
          <span className="dot-sep">{detail.deduplicated} already in Gather</span>
        )}
        {detail.stored > 0 && <span className="dot-sep">{detail.stored} kept as they are</span>}
        {detail.skipped + detail.failed > 0 && (
          <span className="dot-sep">{detail.skipped + detail.failed} skipped</span>
        )}
        {detail.left_out > 0 && (
          <span className="dot-sep">{plural(detail.left_out, "folder")} left out</span>
        )}
        <span className="dot-sep">{sizeLabel(detail.bytes)}</span>
      </p>
      {error && <Callout>{error}</Callout>}

      <Panel
        title={filter === "all" ? "Folders and files" : "Not read"}
        icon={FolderTree}
        className="doc-panel"
        actions={
          skipped.length > 0 && (
            <Segmented
              label="Show"
              value={filter}
              onChange={setFilter}
              options={[
                { value: "all", label: "All" },
                { value: "skipped", label: "Not read", count: skipped.length },
              ]}
            />
          )
        }
      >
        {filter === "all" ? (
          <div className="tree-wrap">
            <Tree
              parent={null}
              children={children}
              counts={counts}
              expanded={expanded ?? new Set()}
              onToggle={toggle}
              onOpenFile={onOpenFile}
            />
          </div>
        ) : (
          <ul className="tree tree-flat">
            {skipped.map((item) => (
              <li key={item.id}>
                <div className={`tree-row tree-file ${item.status}`}>
                  <span className="tree-icon" aria-hidden>
                    {isLeftOut(item) ? <FolderMinus /> : <CircleSlash />}
                  </span>
                  <span className="tree-name">{isLeftOut(item) ? `${item.path}/` : item.path}</span>
                  <span className="tree-status">
                    {isLeftOut(item) ? `Left out: ${item.detail}` : (item.detail ?? "Skipped")}
                  </span>
                </div>
              </li>
            ))}
          </ul>
        )}
      </Panel>

      <SimilarPanel id={id} refreshKey={refreshKey} onSelect={onSelectProject} />

      <div className="inspector-actions">
        {onShowGraph && (
          <Button variant="secondary" icon={Waypoints} onClick={() => onShowGraph(id)}>
            Show as graph
          </Button>
        )}
        <span className="hint">Removing the project keeps its files in Gather.</span>
        <span className="spacer" />
        {confirming ? (
          <>
            <Button variant="ghost" onClick={() => setConfirming(false)} disabled={removing}>
              Keep it
            </Button>
            <Button variant="danger" icon={Trash2} loading={removing} onClick={remove}>
              Remove project
            </Button>
          </>
        ) : (
          <Button variant="ghost" icon={Trash2} onClick={() => setConfirming(true)}>
            Remove project…
          </Button>
        )}
      </div>
    </div>
  );
}

export default function Projects({
  selected,
  onSelect,
  onOpenFile,
  onAddFolder,
  onAddZip,
  onShowGraph,
  refreshKey,
}: {
  selected: string | null;
  onSelect: (id: string | null) => void;
  onOpenFile: (artifactId: string) => void;
  onAddFolder: () => void;
  onAddZip: () => void;
  /** Show a project's own graph. */
  onShowGraph?: (id: string) => void;
  /** Changes whenever an upload finishes. */
  refreshKey: number;
}) {
  const [projects, setProjects] = useState<ProjectSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [removedKey, setRemovedKey] = useState(0);
  // Projects seen so far, so one that appears after an upload is opened.
  const known = useRef<Set<string> | null>(null);

  useEffect(() => {
    let cancelled = false;
    listProjects()
      .then((list) => {
        if (cancelled) return;
        setProjects(list);
        setError(null);
      })
      .catch((e) => !cancelled && setError(errorText(e)));
    return () => {
      cancelled = true;
    };
  }, [refreshKey, removedKey]);

  useEffect(() => {
    if (!projects || projects.length === 0) return;
    const before = known.current;
    known.current = new Set(projects.map((p) => p.id));
    const added = before ? projects.find((p) => !before.has(p.id)) : undefined;
    if (added) onSelect(added.id);
    else if (!selected || !projects.some((p) => p.id === selected)) onSelect(projects[0].id);
  }, [projects, selected, onSelect]);

  const listRef = useListKeys(projects ?? [], selected, keyOf, onSelect);
  const actions = (
    <>
      <Button variant="secondary" icon={FileArchive} onClick={onAddZip}>
        Import .zip
      </Button>
      <Button variant="primary" icon={FolderPlus} onClick={onAddFolder}>
        Add folder
      </Button>
    </>
  );

  return (
    <>
      <Toolbar
        title="Projects"
        icon={FolderTree}
        count={projects && projects.length > 0 ? projects.length : undefined}
      >
        {actions}
      </Toolbar>
      {error && (
        <div className="view-callout">
          <Callout title="Couldn't load projects">{error}</Callout>
        </div>
      )}
      {projects !== null && projects.length === 0 && !error ? (
        <EmptyState
          icon={FolderTree}
          title="No projects yet"
          action={<div className="empty-actions">{actions}</div>}
        >
          Add a whole folder, or a .zip of one, and Gather reads every file in it and keeps the
          folders as they were. You can also drop a folder anywhere in the window.
        </EmptyState>
      ) : (
        <SplitView
          listLabel="Projects"
          list={
            projects === null ? (
              <Skeleton rows={5} />
            ) : (
              <ul className="rows" ref={listRef}>
                {projects.map((p) => (
                  <li key={p.id}>
                    <button
                      type="button"
                      className="row"
                      aria-current={p.id === selected ? "true" : undefined}
                      onClick={() => onSelect(p.id)}
                    >
                      <span className="row-lead" aria-hidden>
                        {p.source === "zip" ? <FileArchive /> : <FolderTree />}
                      </span>
                      <span className="row-main">
                        <span className="row-title">{p.name}</span>
                        <span className="row-meta">{summaryLine(p)}</span>
                      </span>
                      <span className="row-trail">
                        <When iso={p.updated_at} className="hint" />
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )
          }
          detail={
            selected ? (
              <ProjectView
                id={selected}
                refreshKey={refreshKey}
                onOpenFile={onOpenFile}
                onSelectProject={onSelect}
                onShowGraph={onShowGraph}
                onRemoved={() => {
                  onSelect(null);
                  setRemovedKey((k) => k + 1);
                }}
              />
            ) : null
          }
        />
      )}
    </>
  );
}
