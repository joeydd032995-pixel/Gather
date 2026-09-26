import { useEffect, useState } from "react";
import { CircleCheck, Layers, Undo2 } from "lucide-react";
import {
  getCluster,
  unmergeEntity,
  type ClusterKind,
  type ClusterMember,
  type ClusterSummary,
} from "./api";
import { useAsync } from "./hooks/useAsync";
import { useListKeys } from "./hooks/useListKeys";
import { usePagedClusters } from "./hooks/usePagedClusters";
import { plural } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  Meter,
  Panel,
  Segmented,
  Skeleton,
  SplitView,
  Toolbar,
  errorText,
} from "./ui";

const KINDS: { value: ClusterKind; label: string }[] = [
  { value: "topic", label: "Topics" },
  { value: "entity", label: "Merged duplicates" },
];

const keyOf = (c: ClusterSummary) => c.id;

function memberLabel(m: ClusterMember): string {
  return m.statement ?? m.name ?? m.filename ?? m.member_id;
}

function ClusterDetail({ cluster }: { cluster: ClusterSummary }) {
  const detail = useAsync(() => getCluster(cluster.id), [cluster.id]);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [undone, setUndone] = useState(false);

  const undoMerge = async (member: ClusterMember) => {
    setBusyId(member.member_id);
    setError(null);
    try {
      await unmergeEntity(member.member_id, "undone from the Groups view");
      setUndone(true);
      detail.reload();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusyId(null);
    }
  };

  const isEntity = cluster.kind === "entity";
  return (
    <div className="inspector" key={cluster.id}>
      <div className="inspector-kicker">
        <Badge tone="accent" icon={Layers}>
          {isEntity ? "Merged duplicates" : "Topic"}
        </Badge>
        <Meter
          value={cluster.cohesion}
          label="Cohesion: how alike the members are"
          tone="neutral"
          width={64}
        />
      </div>
      <h2 className="inspector-title">{cluster.label || "(unlabelled)"}</h2>
      <p className="inspector-sub">
        {plural(cluster.size, isEntity ? "name" : "statement")}
        {isEntity && <span className="dot-sep">You can split any of them back out</span>}
      </p>

      {error && <Callout>{error}</Callout>}

      <Panel title={isEntity ? "Names" : "Statements"} className="doc-panel">
        {detail.loading && !detail.data ? (
          <Skeleton rows={3} />
        ) : detail.error && undone ? (
          <div className="panel-pad">
            <Callout tone="success" icon={CircleCheck}>
              Merge undone; this group no longer exists.
            </Callout>
          </div>
        ) : detail.error ? (
          <div className="panel-pad">
            <Callout>{detail.error}</Callout>
          </div>
        ) : (
          <ul className="members">
            {(detail.data?.members ?? []).map((m) => (
              <li key={m.member_id} className="member">
                <span className="member-text">
                  {m.member_kind === "entity" ? (
                    <>
                      <span className="member-name">{memberLabel(m)}</span>
                      {!m.merged_into && <Badge tone="accent">kept</Badge>}
                    </>
                  ) : (
                    memberLabel(m)
                  )}
                </span>
                {m.member_kind === "entity" && m.merged_into && (
                  <Button
                    variant="ghost"
                    size="sm"
                    icon={Undo2}
                    onClick={() => undoMerge(m)}
                    loading={busyId === m.member_id}
                    disabled={busyId !== null}
                    title="Split this entity back out; the pair will never be merged again"
                  >
                    Undo merge
                  </Button>
                )}
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}

/** How the pipeline arranged the brain: topic groups and merged duplicates. */
export default function Clusters() {
  const [kind, setKind] = useState<ClusterKind>("topic");
  const [selected, setSelected] = useState<string | null>(null);
  const clusters = usePagedClusters(kind);

  useEffect(() => {
    if (clusters.items.length === 0) return;
    if (!selected || !clusters.items.some((c) => c.id === selected))
      setSelected(clusters.items[0].id);
  }, [clusters.items, selected]);

  const listRef = useListKeys(clusters.items, selected, keyOf, setSelected);
  const current = clusters.items.find((c) => c.id === selected);

  return (
    <>
      <Toolbar title="Groups" icon={Layers} count={clusters.items.length || undefined}>
        <Segmented
          label="Group type"
          options={KINDS}
          value={kind}
          onChange={(k) => {
            setKind(k);
            setSelected(null);
          }}
        />
      </Toolbar>
      {clusters.error && (
        <div className="view-callout">
          <Callout title="Couldn't load groups">{clusters.error}</Callout>
        </div>
      )}
      {!clusters.loading && !clusters.error && clusters.items.length === 0 ? (
        <EmptyState icon={Layers} title="Nothing grouped yet">
          Groups appear as the clustering worker runs in the background.
        </EmptyState>
      ) : (
        <SplitView
          listLabel="Groups"
          list={
            clusters.loading && clusters.items.length === 0 ? (
              <Skeleton rows={5} />
            ) : (
              <>
                <ul className="rows" ref={listRef}>
                  {clusters.items.map((c) => (
                    <li key={c.id}>
                      <button
                        type="button"
                        className="row"
                        aria-current={c.id === selected ? "true" : undefined}
                        onClick={() => setSelected(c.id)}
                      >
                        <span className="row-lead size-lead num" aria-hidden>
                          {c.size}
                        </span>
                        <span className="row-main">
                          <span className="row-title">{c.label || "(unlabelled)"}</span>
                          <span className="row-meta">
                            {plural(c.size, c.kind === "entity" ? "name" : "statement")}
                          </span>
                        </span>
                        <span className="row-trail">
                          <span
                            className="num row-score"
                            title="Cohesion: how alike the members are"
                          >
                            {c.cohesion.toFixed(2)}
                          </span>
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
                {clusters.hasMore && (
                  <div className="list-more">
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={clusters.loadMore}
                      loading={clusters.loading}
                    >
                      Load more
                    </Button>
                  </div>
                )}
              </>
            )
          }
          detail={current ? <ClusterDetail cluster={current} /> : null}
        />
      )}
    </>
  );
}
