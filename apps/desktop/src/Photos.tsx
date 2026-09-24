import { useState } from "react";
import { getCluster, type ClusterKind, type ClusterSummary } from "./api";
import { useAsync } from "./hooks/useAsync";
import { usePagedClusters } from "./hooks/usePagedClusters";
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
/** Thumbnails shown per expanded group before "Show more" (each is a local decode). */
const PHOTOS_PER_PAGE = 48;

function GroupPhotos({ cluster }: { cluster: ClusterSummary }) {
  const detail = useAsync(() => getCluster(cluster.id), [cluster.id]);
  const [shown, setShown] = useState(PHOTOS_PER_PAGE);
  if (detail.loading) return <p className="prov-empty">Loading…</p>;
  if (detail.error) return <p className="error">{detail.error}</p>;
  const members = detail.data?.members ?? [];
  // Only a duplicate group's representative is its sharpest copy; for albums
  // and topics it is just the first photo, so it gets no badge.
  const isDuplicateGroup = cluster.kind === "photo_dup";
  return (
    <>
      <div className="photo-grid">
        {members.slice(0, shown).map((m) => (
          <figure key={m.member_id} className="photo">
            <Thumbnail imageId={m.member_id} alt={m.filename ?? "photo"} size={PREVIEW_SIZE} />
            <figcaption>
              {isDuplicateGroup && m.member_id === cluster.representative_id && (
                <span className="prov-badge">best</span>
              )}
              {m.caption ?? m.filename ?? ""}
            </figcaption>
          </figure>
        ))}
      </div>
      {members.length > shown && (
        <button className="load-more" onClick={() => setShown((n) => n + PHOTOS_PER_PAGE)}>
          Show more ({members.length - shown} left)
        </button>
      )}
    </>
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
  const groups = usePagedClusters(kind);
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
      {groups.error && <p className="error">{groups.error}</p>}
      {!groups.loading && groups.items.length === 0 && (
        <p className="prov-empty">{current?.empty}</p>
      )}
      <ul className="photo-groups">
        {groups.items.map((c) => (
          <PhotoGroup key={c.id} cluster={c} />
        ))}
      </ul>
      {groups.loading && <p>Loading…</p>}
      {groups.hasMore && !groups.loading && (
        <button className="load-more" onClick={groups.loadMore}>
          Load more
        </button>
      )}
    </section>
  );
}
