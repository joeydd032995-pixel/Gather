import { ArrowRight, ArrowUpRight, FileText } from "lucide-react";
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

function Monogram({ node }: { node: Node }) {
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
