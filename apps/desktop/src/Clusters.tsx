import { useState } from "react";
import { getCluster, type ClusterKind, type ClusterMember, type ClusterSummary } from "./api";
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
  if (detail.loading) return <p className="prov-empty">Loading…</p>;
  if (detail.error) return <p className="error">{detail.error}</p>;
  return (
    <ul className="prov-list">
      {(detail.data?.members ?? []).map((m) => (
        <li key={m.member_id}>{memberLabel(m)}</li>
      ))}
    </ul>
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
