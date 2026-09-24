import { useState } from "react";
import {
  getCluster,
  unmergeEntity,
  type ClusterKind,
  type ClusterMember,
  type ClusterSummary,
} from "./api";
import { useAsync } from "./hooks/useAsync";
import { usePagedClusters } from "./hooks/usePagedClusters";

const KINDS: { kind: ClusterKind; label: string }[] = [
  { kind: "topic", label: "Topics" },
  { kind: "entity", label: "Merged duplicates" },
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
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusyId(null);
    }
  };

  if (detail.loading && !detail.data) return <p className="prov-empty">Loading…</p>;
  // Undoing the last merge in a pair dissolves the group itself.
  if (detail.error && undone) {
    return <p className="all-clear">Merge undone; this group no longer exists.</p>;
  }
  if (detail.error) return <p className="error">{detail.error}</p>;
  return (
    <>
      {error && <p className="error">{error}</p>}
      <ul className="prov-list">
        {(detail.data?.members ?? []).map((m) => (
          <li key={m.member_id}>
            {memberLabel(m)}
            {m.member_kind === "entity" && m.merged_into && (
              <>
                {" "}
                <button
                  className="link-button"
                  onClick={() => undoMerge(m)}
                  disabled={busyId !== null}
                  title="Split this entity back out; the pair will never be merged again"
                >
                  {busyId === m.member_id ? "Undoing…" : "Undo merge"}
                </button>
              </>
            )}
          </li>
        ))}
      </ul>
    </>
  );
}

function ClusterRow({ cluster }: { cluster: ClusterSummary }) {
  const [open, setOpen] = useState(false);
  return (
    <li className="conflict-item">
      <button className="conflict-row" onClick={() => setOpen((o) => !o)}>
        <span className="score" title="members">
          {cluster.size}
        </span>
        <span className="statements">{cluster.label || "(unlabelled)"}</span>
        <span className="method" title="cohesion: mean similarity inside the group">
          {cluster.cohesion.toFixed(2)}
        </span>
      </button>
      {open && (
        <div className="conflict-detail">
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
      <nav className="subtabs">
        {KINDS.map((k) => (
          <button
            key={k.kind}
            className={kind === k.kind ? "tab active" : "tab"}
            onClick={() => setKind(k.kind)}
          >
            {k.label}
          </button>
        ))}
      </nav>
      {clusters.error && <p className="error">{clusters.error}</p>}
      {!clusters.loading && clusters.items.length === 0 && (
        <p className="prov-empty">Nothing grouped yet; groups appear as the clustering worker runs.</p>
      )}
      <ul className="conflict-list">
        {clusters.items.map((c) => (
          <ClusterRow key={c.id} cluster={c} />
        ))}
      </ul>
      {clusters.loading && <p>Loading…</p>}
      {clusters.hasMore && !clusters.loading && (
        <button className="load-more" onClick={clusters.loadMore}>
          Load more
        </button>
      )}
    </section>
  );
}
