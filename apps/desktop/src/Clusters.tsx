import { useState } from "react";
import { ChevronRight, CircleCheck, Layers, Undo2 } from "lucide-react";
import {
  getCluster,
  unmergeEntity,
  type ClusterKind,
  type ClusterMember,
  type ClusterSummary,
} from "./api";
import { useAsync } from "./hooks/useAsync";
import { usePagedClusters } from "./hooks/usePagedClusters";
import { plural } from "./kinds";
import {
  Badge,
  Button,
  Callout,
  EmptyState,
  KindTag,
  Meter,
  PageHeader,
  Segmented,
  Skeleton,
  errorText,
} from "./ui";

const KINDS: { value: ClusterKind; label: string }[] = [
  { value: "topic", label: "Topics" },
  { value: "entity", label: "Merged duplicates" },
];

function memberLabel(m: ClusterMember): string {
  return m.statement ?? m.name ?? m.filename ?? m.member_id;
}

function ClusterMembers({ id }: { id: string }) {
  const detail = useAsync(() => getCluster(id), [id]);
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

  if (detail.loading && !detail.data) return <Skeleton rows={3} />;
  // Undoing the last merge in a pair dissolves the group itself.
  if (detail.error && undone) {
    return (
      <Callout tone="success" icon={CircleCheck}>
        Merge undone; this group no longer exists.
      </Callout>
    );
  }
  if (detail.error) return <Callout>{detail.error}</Callout>;
  return (
    <>
      {error && <Callout>{error}</Callout>}
      <ul className="members">
        {(detail.data?.members ?? []).map((m) => (
          <li key={m.member_id} className="member">
            {m.member_kind === "entity" ? (
              <span className="member-text">
                <span className="item-title">{memberLabel(m)}</span>
                {!m.merged_into && (
                  <Badge tone="accent" className="member-badge">
                    kept
                  </Badge>
                )}
              </span>
            ) : (
              <span className="member-text">{memberLabel(m)}</span>
            )}
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
    </>
  );
}

function ClusterRow({ cluster, index }: { cluster: ClusterSummary; index: number }) {
  const [open, setOpen] = useState(false);
  return (
    <li
      className={open ? "item open" : "item"}
      style={{ animationDelay: `${Math.min(index, 8) * 30}ms` }}
    >
      <button
        type="button"
        className="item-row"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <span className="size-chip num" title="members">
          {cluster.size}
        </span>
        <div className="item-main">
          <p className="item-title">{cluster.label || "(unlabelled)"}</p>
          <div className="item-sub">
            {cluster.kind === "entity" ? (
              <KindTag kind="other" label={plural(cluster.size, "name")} />
            ) : (
              <span>{plural(cluster.size, "statement")}</span>
            )}
          </div>
        </div>
        <div className="item-aside">
          <Meter
            value={cluster.cohesion}
            label="Cohesion: how alike the members are"
            tone="neutral"
          />
          <ChevronRight className="chevron" aria-hidden />
        </div>
      </button>
      {open && (
        <div className="item-body">
          <ClusterMembers id={cluster.id} />
        </div>
      )}
    </li>
  );
}

/** How the pipeline arranged the brain: topic groups and merged duplicates. */
export default function Clusters() {
  const [kind, setKind] = useState<ClusterKind>("topic");
  const clusters = usePagedClusters(kind);

  return (
    <section>
      <PageHeader
        title="Groups"
        description="How Gather arranged your library on its own: related statements gathered into topics, and names it recognised as the same thing."
        actions={<Segmented label="Group type" options={KINDS} value={kind} onChange={setKind} />}
      />
      {clusters.error && <Callout title="Couldn't load groups">{clusters.error}</Callout>}
      {!clusters.loading && !clusters.error && clusters.items.length === 0 && (
        <EmptyState icon={Layers} title="Nothing grouped yet">
          Groups appear as the clustering worker runs in the background.
        </EmptyState>
      )}
      {clusters.loading && clusters.items.length === 0 ? (
        <Skeleton rows={4} variant="card" />
      ) : (
        <ul className="stack">
          {clusters.items.map((c, i) => (
            <ClusterRow key={c.id} cluster={c} index={i} />
          ))}
        </ul>
      )}
      {clusters.hasMore && (
        <div className="load-more">
          <Button onClick={clusters.loadMore} loading={clusters.loading}>
            Load more
          </Button>
        </div>
      )}
    </section>
  );
}
