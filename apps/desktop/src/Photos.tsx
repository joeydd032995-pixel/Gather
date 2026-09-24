import { useState } from "react";
import { getCluster, listClusters, type ClusterKind, type ClusterSummary } from "./api";
import { useAsync } from "./hooks/useAsync";
import Thumbnail from "./Thumbnail";

const KINDS: { kind: ClusterKind; label: string; empty: string }[] = [
  {
    kind: "album",
    label: "Albums",
    empty: "No albums yet. Albums form from photos with EXIF capture times.",
  },
  {
    kind: "photo_dup",
    label: "Duplicates",
    empty: "No near-duplicate photos found.",
  },
  {
    kind: "photo_topic",
    label: "Visual topics",
    empty: "No visual topics. They need a local vision model (GATHER_OLLAMA_VISION_MODEL).",
  },
];

const PREVIEW_SIZE = 96;
const COVER_SIZE = 160;

function GroupPhotos({ cluster }: { cluster: ClusterSummary }) {
  const detail = useAsync(() => getCluster(cluster.id), [cluster.id]);
  if (detail.loading) return <p className="prov-empty">Loading…</p>;
  if (detail.error) return <p className="error">{detail.error}</p>;
  return (
    <div className="photo-grid">
      {(detail.data?.members ?? []).map((m) => (
        <figure key={m.member_id} className="photo">
          <Thumbnail imageId={m.member_id} alt={m.filename ?? "photo"} size={PREVIEW_SIZE} />
          <figcaption>
            {m.member_id === cluster.representative_id && <span className="prov-badge">best</span>}
            {m.caption ?? m.filename ?? ""}
          </figcaption>
        </figure>
      ))}
    </div>
  );
}

function PhotoGroup({ cluster }: { cluster: ClusterSummary }) {
  const [open, setOpen] = useState(false);
  return (
    <li className="photo-group">
      <button className="photo-cover" onClick={() => setOpen((o) => !o)}>
        {cluster.representative_id ? (
          <Thumbnail imageId={cluster.representative_id} alt={cluster.label} size={COVER_SIZE} />
        ) : (
          <div className="thumb" style={{ width: COVER_SIZE, height: COVER_SIZE }} />
        )}
        <span className="photo-label">
          {cluster.label} · {cluster.size}
        </span>
      </button>
      {open && <GroupPhotos cluster={cluster} />}
    </li>
  );
}

/** Photos, organised automatically. Nothing here was deleted: duplicates are
 *  grouped with the sharpest copy marked "best". */
export default function Photos() {
  const [kind, setKind] = useState<ClusterKind>("album");
  const groups = useAsync(() => listClusters(kind), [kind]);
  const current = KINDS.find((k) => k.kind === kind);

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
      {groups.loading && !groups.data && <p>Loading…</p>}
      {groups.error && <p className="error">{groups.error}</p>}
      {groups.data?.length === 0 && <p className="prov-empty">{current?.empty}</p>}
      <ul className="photo-groups">
        {(groups.data ?? []).map((c) => (
          <PhotoGroup key={c.id} cluster={c} />
        ))}
      </ul>
    </section>
  );
}
