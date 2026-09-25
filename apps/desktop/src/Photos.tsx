import { useState } from "react";
import { ArrowLeft, Images, Star } from "lucide-react";
import { getCluster, type ClusterKind, type ClusterSummary } from "./api";
import { useAsync } from "./hooks/useAsync";
import { usePagedClusters } from "./hooks/usePagedClusters";
import { plural } from "./kinds";
import Thumbnail from "./Thumbnail";
import { Badge, Button, Callout, EmptyState, PageHeader, Segmented } from "./ui";

const KINDS: { value: ClusterKind; label: string; empty: string }[] = [
  {
    value: "album",
    label: "Albums",
    empty: "Albums form on their own from photos with EXIF capture times.",
  },
  {
    value: "photo_dup",
    label: "Duplicates",
    empty: "No near-duplicate photos found.",
  },
  {
    value: "photo_topic",
    label: "Visual topics",
    empty: "Visual topics need a local vision model (GATHER_OLLAMA_VISION_MODEL).",
  },
];

/** Thumbnails shown per open group before "Show more" (each is a local decode). */
const PHOTOS_PER_PAGE = 48;

function GroupPhotos({ cluster, onBack }: { cluster: ClusterSummary; onBack: () => void }) {
  const detail = useAsync(() => getCluster(cluster.id), [cluster.id]);
  const [shown, setShown] = useState(PHOTOS_PER_PAGE);
  const members = detail.data?.members ?? [];
  // Only a duplicate group's representative is its sharpest copy; for albums
  // and topics it is just the first photo, so it gets no badge.
  const isDuplicateGroup = cluster.kind === "photo_dup";
  return (
    <section className="album" aria-labelledby="album-title">
      <div className="album-head">
        <Button variant="ghost" size="sm" icon={ArrowLeft} onClick={onBack} className="back">
          All groups
        </Button>
        <h2 className="album-title" id="album-title">
          {cluster.label}
        </h2>
        <p className="hint">
          {plural(cluster.size, "photo")}
          {isDuplicateGroup && " · the sharpest copy is marked best; nothing was deleted"}
        </p>
      </div>
      {detail.loading && !detail.data ? (
        <div className="photo-grid">
          {Array.from({ length: Math.min(cluster.size, 12) }, (_, i) => (
            <div className="thumb" key={i} />
          ))}
        </div>
      ) : detail.error ? (
        <Callout>{detail.error}</Callout>
      ) : (
        <>
          <ul className="photo-grid">
            {members.slice(0, shown).map((m) => {
              const best = isDuplicateGroup && m.member_id === cluster.representative_id;
              return (
                <li key={m.member_id}>
                  <figure className={best ? "photo best" : "photo"}>
                    <Thumbnail imageId={m.member_id} alt={m.caption ?? m.filename ?? "photo"} />
                    {best && (
                      <Badge tone="accent" icon={Star} className="photo-best">
                        Best
                      </Badge>
                    )}
                    <figcaption title={m.caption ?? m.filename ?? ""}>
                      {m.caption ?? m.filename ?? ""}
                    </figcaption>
                  </figure>
                </li>
              );
            })}
          </ul>
          {members.length > shown && (
            <div className="load-more">
              <Button onClick={() => setShown((n) => n + PHOTOS_PER_PAGE)}>
                Show more · {members.length - shown} left
              </Button>
            </div>
          )}
        </>
      )}
    </section>
  );
}

/** Photos, organised automatically. Nothing here was deleted: duplicates are
 *  grouped with the sharpest copy marked "best". */
export default function Photos() {
  const [kind, setKind] = useState<ClusterKind>("album");
  const [open, setOpen] = useState<ClusterSummary | null>(null);
  const groups = usePagedClusters(kind);
  const current = KINDS.find((k) => k.value === kind);

  return (
    <section>
      <PageHeader
        title="Photos"
        description="Organised automatically. Nothing is ever deleted: duplicates are grouped with the sharpest copy on top."
        actions={
          <Segmented
            label="Photo grouping"
            options={KINDS}
            value={kind}
            onChange={(k) => {
              setKind(k);
              setOpen(null);
            }}
          />
        }
      />
      {open ? (
        <GroupPhotos cluster={open} onBack={() => setOpen(null)} />
      ) : (
        <>
          {groups.error && <Callout title="Couldn't load photos">{groups.error}</Callout>}
          {!groups.loading && !groups.error && groups.items.length === 0 && (
            <EmptyState icon={Images} title={`No ${current?.label.toLowerCase() ?? "groups"} yet`}>
              {current?.empty}
            </EmptyState>
          )}
          {groups.loading && groups.items.length === 0 ? (
            <div className="album-grid">
              {Array.from({ length: 4 }, (_, i) => (
                <div className="album-card skeleton-card-photo" key={i}>
                  <div className="thumb" />
                </div>
              ))}
            </div>
          ) : (
            <ul className="album-grid">
              {groups.items.map((c, i) => (
                <li key={c.id} style={{ animationDelay: `${Math.min(i, 8) * 40}ms` }}>
                  <button
                    type="button"
                    className="album-card"
                    data-album
                    onClick={() => setOpen(c)}
                  >
                    <span className="album-cover">
                      {c.representative_id ? (
                        <Thumbnail imageId={c.representative_id} alt="" />
                      ) : (
                        <span className="thumb" />
                      )}
                      <span className="album-count num">{c.size}</span>
                    </span>
                    <span className="album-label">{c.label}</span>
                    <span className="hint">{plural(c.size, "photo")}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
          {groups.hasMore && (
            <div className="load-more">
              <Button onClick={groups.loadMore} loading={groups.loading}>
                Load more
              </Button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
