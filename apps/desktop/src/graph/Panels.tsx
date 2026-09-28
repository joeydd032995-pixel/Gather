import {
  ArrowRight,
  ArrowUpRight,
  FileArchive,
  FileText,
  Folder,
  FolderArchive,
  FolderTree,
  Waypoints,
} from "lucide-react";
import { kindIcon, kindLabel, plural } from "../kinds";
import { Button, KindTag } from "../ui";
import UnitList from "../UnitList";
import { endpoint, kindKey, type Graph, type Node } from "./model";

interface PanelProps {
  node: Node;
  graph: Graph;
  onSelect: (key: string) => void;
  onOpenFile: (id: string) => void;
}

interface ProjectPanelProps extends PanelProps {
  /** Open the project in the Projects view. */
  onOpenProject?: (id: string) => void;
  /** Show the project's own graph; absent when this is that graph. */
  onShowProjectGraph?: (id: string) => void;
}

const linksOf = (graph: Graph, node: Node) =>
  graph.links.filter(
    (l) => endpoint(l.source)?.key === node.key || endpoint(l.target)?.key === node.key,
  );

/** What a project or folder holds, folders first. */
function children(graph: Graph, node: Node): Node[] {
  return graph.links
    .filter((l) => l.type === "contains" && endpoint(l.source)?.key === node.key)
    .map((l) => endpoint(l.target))
    .filter((n): n is Node => n !== null)
    .sort((a, b) =>
      a.type !== b.type ? (a.type === "folder" ? -1 : 1) : a.name.localeCompare(b.name),
    );
}

/** Projects and folders holding `node`. */
function parents(graph: Graph, node: Node): Node[] {
  return graph.links
    .filter((l) => l.type === "contains" && endpoint(l.target)?.key === node.key)
    .map((l) => endpoint(l.source))
    .filter((n): n is Node => n !== null);
}

function NodeIcon({ node }: { node: Node }) {
  if (node.type === "project") return <FolderTree className="relation-icon" aria-hidden />;
  if (node.type === "folder") {
    const Icon = node.kind === "zip" ? FolderArchive : Folder;
    return <Icon className="relation-icon" aria-hidden />;
  }
  if (node.type === "file") return <FileText className="relation-icon" aria-hidden />;
  return <span className="relation-dot" data-kind={kindKey(node.kind)} aria-hidden />;
}

function Contents({
  items,
  onSelect,
  onOpenFile,
}: {
  items: Node[];
  onSelect: (key: string) => void;
  onOpenFile: (id: string) => void;
}) {
  return (
    <ul className="relations">
      {items.map((c) => (
        <li key={c.key}>
          <button
            type="button"
            className="relation"
            onClick={() => (c.type === "file" ? onOpenFile(c.id) : onSelect(c.key))}
          >
            <NodeIcon node={c} />
            <span className="relation-text">{c.name}</span>
            {c.type === "file" ? (
              <ArrowUpRight className="relation-go" aria-hidden />
            ) : (
              <ArrowRight className="relation-go" aria-hidden />
            )}
          </button>
        </li>
      ))}
    </ul>
  );
}

function Monogram({ node }: { node: Node }) {
  if (node.type === "project" || node.type === "folder") {
    const Icon =
      node.type === "project"
        ? node.kind === "zip"
          ? FileArchive
          : FolderTree
        : node.kind === "zip"
          ? FolderArchive
          : Folder;
    return (
      <span className="monogram monogram-project" data-type={node.type} aria-hidden>
        <Icon />
      </span>
    );
  }
  if (node.type === "file") {
    const Icon = kindIcon(node.kind);
    return (
      <span className="monogram monogram-file" aria-hidden>
        <Icon />
      </span>
    );
  }
  const initials = node.name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0]!.toUpperCase())
    .join("");
  return (
    <span className="monogram" data-kind={kindKey(node.kind)} aria-hidden>
      {initials || "?"}
    </span>
  );
}

function Stat({ value, label }: { value: number; label: string }) {
  return (
    <div className="drawer-stat">
      <span className="drawer-stat-value num">{value}</span>
      <span className="drawer-stat-label">{label}</span>
    </div>
  );
}

export function EntityPanel({ node, graph, onSelect, onOpenFile }: PanelProps) {
  const touching = graph.links.filter(
    (l) => endpoint(l.source)?.key === node.key || endpoint(l.target)?.key === node.key,
  );
  const relations = touching.filter((l) => l.type === "relation");
  const files = touching
    .filter((l) => l.type === "mention")
    .map((l) => endpoint(l.source))
    .filter((n): n is Node => n !== null);

  return (
    <>
      <div className="drawer-hero">
        <Monogram node={node} />
        <div className="drawer-hero-text">
          <KindTag kind={kindKey(node.kind)} label={node.kind} />
          <h2 className="drawer-title">{node.name}</h2>
        </div>
      </div>
      <div className="drawer-stats">
        <Stat
          value={relations.length}
          label={relations.length === 1 ? "connection" : "connections"}
        />
        <Stat value={files.length} label={files.length === 1 ? "file" : "files"} />
        <Stat value={node.weight} label="weight" />
      </div>

      {relations.length > 0 && (
        <section className="drawer-section">
          <h3 className="section-label">Connections</h3>
          <ul className="relations">
            {relations.map((l, i) => {
              const source = endpoint(l.source)!;
              const target = endpoint(l.target)!;
              const outgoing = source.key === node.key;
              const other = outgoing ? target : source;
              return (
                <li key={i}>
                  <button type="button" className="relation" onClick={() => onSelect(other.key)}>
                    <span className="relation-dot" data-kind={kindKey(other.kind)} aria-hidden />
                    <span className="relation-text">
                      {outgoing ? (
                        <>
                          <span className="relation-verb">{l.label}</span> {other.name}
                        </>
                      ) : (
                        <>
                          {other.name} <span className="relation-verb">{l.label} this</span>
                        </>
                      )}
                    </span>
                    <ArrowRight className="relation-go" aria-hidden />
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      )}

      {files.length > 0 && (
        <section className="drawer-section">
          <h3 className="section-label">Mentioned in</h3>
          <ul className="relations">
            {files.map((f) => (
              <li key={f.key}>
                <button type="button" className="relation" onClick={() => onOpenFile(f.id)}>
                  <FileText className="relation-icon" aria-hidden />
                  <span className="relation-text">{f.name}</span>
                  <ArrowUpRight className="relation-go" aria-hidden />
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      <section className="drawer-section">
        <h3 className="section-label">What Gather knows</h3>
        <UnitList
          subjectEntityId={node.id}
          pageSize={50}
          compact
          empty={
            <p className="hint">
              No statements are about this one directly; it appears in others'.
            </p>
          }
        />
      </section>
    </>
  );
}

export function FilePanel({ node, graph, onSelect, onOpenFile }: PanelProps) {
  const heldBy = parents(graph, node);
  const mentioned = graph.links
    .filter((l) => l.type === "mention" && endpoint(l.source)?.key === node.key)
    .map((l) => endpoint(l.target))
    .filter((n): n is Node => n !== null);
  return (
    <>
      <div className="drawer-hero">
        <Monogram node={node} />
        <div className="drawer-hero-text">
          <KindTag kind="file" label={kindLabel(node.kind)} />
          <h2 className="drawer-title">{node.name}</h2>
        </div>
      </div>
      <Button
        variant="primary"
        icon={ArrowUpRight}
        onClick={() => onOpenFile(node.id)}
        className="drawer-cta"
      >
        Open in Library
      </Button>
      {heldBy.length > 0 && (
        <section className="drawer-section">
          <h3 className="section-label">In</h3>
          <Contents items={heldBy} onSelect={onSelect} onOpenFile={onOpenFile} />
        </section>
      )}
      <section className="drawer-section">
        <h3 className="section-label">
          Mentions <span className="count">{plural(mentioned.length, "entity", "entities")}</span>
        </h3>
        <ul className="relations">
          {mentioned.map((e) => (
            <li key={e.key}>
              <button type="button" className="relation" onClick={() => onSelect(e.key)}>
                <span className="relation-dot" data-kind={kindKey(e.kind)} aria-hidden />
                <span className="relation-text">{e.name}</span>
                <ArrowRight className="relation-go" aria-hidden />
              </button>
            </li>
          ))}
        </ul>
      </section>
    </>
  );
}

export function ProjectPanel({
  node,
  graph,
  onSelect,
  onOpenFile,
  onOpenProject,
  onShowProjectGraph,
}: ProjectPanelProps) {
  const alike = linksOf(graph, node)
    .filter((l) => l.type === "similar")
    .map((l) => {
      const s = endpoint(l.source)!;
      return { other: s.key === node.key ? endpoint(l.target)! : s, link: l };
    })
    .sort((a, b) => (b.link.score ?? 0) - (a.link.score ?? 0));
  const holds = children(graph, node);
  return (
    <>
      <div className="drawer-hero">
        <Monogram node={node} />
        <div className="drawer-hero-text">
          <KindTag kind="project" label={node.kind === "zip" ? "Project from a .zip" : "Project"} />
          <h2 className="drawer-title">{node.name}</h2>
        </div>
      </div>
      <div className="drawer-stats">
        <Stat value={node.weight} label={node.weight === 1 ? "file" : "files"} />
        <Stat value={alike.length} label={alike.length === 1 ? "alike" : "alike"} />
      </div>
      <div className="drawer-actions">
        {onOpenProject && (
          <Button variant="primary" icon={ArrowUpRight} onClick={() => onOpenProject(node.id)}>
            Open project
          </Button>
        )}
        {onShowProjectGraph && (
          <Button variant="secondary" icon={Waypoints} onClick={() => onShowProjectGraph(node.id)}>
            Its graph
          </Button>
        )}
      </div>

      {alike.length > 0 && (
        <section className="drawer-section">
          <h3 className="section-label">Similar projects</h3>
          <ul className="relations">
            {alike.map(({ other, link }) => (
              <li key={other.key}>
                <button
                  type="button"
                  className="relation relation-tall"
                  onClick={() => onSelect(other.key)}
                >
                  <FolderTree className="relation-icon" aria-hidden />
                  <span className="relation-text">
                    {other.name}
                    <span className="relation-why">{link.reasons?.[0]}</span>
                  </span>
                  <span className="relation-score num">{Math.round((link.score ?? 0) * 100)}%</span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      {holds.length > 0 && (
        <section className="drawer-section">
          <h3 className="section-label">
            Holds <span className="count">{plural(holds.length, "item")}</span>
          </h3>
          <Contents items={holds} onSelect={onSelect} onOpenFile={onOpenFile} />
        </section>
      )}
    </>
  );
}

export function FolderPanel({ node, graph, onSelect, onOpenFile }: PanelProps) {
  const holds = children(graph, node);
  const heldBy = parents(graph, node);
  return (
    <>
      <div className="drawer-hero">
        <Monogram node={node} />
        <div className="drawer-hero-text">
          <KindTag kind="file" label={node.kind === "zip" ? "Unpacked .zip" : "Folder"} />
          <h2 className="drawer-title">{node.name}</h2>
        </div>
      </div>
      {heldBy.length > 0 && (
        <section className="drawer-section">
          <h3 className="section-label">In</h3>
          <Contents items={heldBy} onSelect={onSelect} onOpenFile={onOpenFile} />
        </section>
      )}
      <section className="drawer-section">
        <h3 className="section-label">
          Holds <span className="count">{plural(holds.length, "item")}</span>
        </h3>
        <Contents items={holds} onSelect={onSelect} onOpenFile={onOpenFile} />
      </section>
    </>
  );
}
