import { useState } from "react";
import { ArrowLeft, Images, Star } from "lucide-react";
import { getCluster, type ClusterKind, type ClusterSummary } from "./api";
import { useAsync } from "./hooks/useAsync";
import { usePagedClusters } from "./hooks/usePagedClusters";
import { plural } from "./kinds";
import Thumbnail from "./Thumbnail";
import { Badge, Button, Callout, EmptyState, IconButton, Segmented, Toolbar } from "./ui";

const KINDS: { value: ClusterKind; label: string; empty: string }[] = [
  {
    value: "album",
    label: "Albums",
    empty: "Albums form on their own from photos with EXIF capture times.",
  },
  { value: "photo_dup", label: "Duplicates", empty: "No near-duplicate photos found." },
  {
    value: "photo_topic",
    label: "Visual topics",
    empty: "Visual topics need a local vision model (GATHER_OLLAMA_VISION_MODEL).",
  },
];

/** Thumbnails shown per open group before "Show more" (each is a local decode). */
const PHOTOS_PER_PAGE = 48;

function GroupPhotos({ cluster }: { cluster: ClusterSummary }) {
  const detail = useAsync(() => getCluster(cluster.id), [cluster.id]);
  const [shown, setShown] = useState(PHOTOS_PER_PAGE);
  const members = detail.data?.members ?? [];
  // Only a duplicate group's representative is its sharpest copy; for albums
  // and topics it is just the first photo, so it gets no badge.
  const isDuplicateGroup = cluster.kind === "photo_dup";
  if (detail.error) return <Callout>{detail.error}</Callout>;
  return (
    <>
      {isDuplicateGroup && (
        <p className="hint album-note">The sharpest copy is marked best. Nothing was deleted.</p>
      )}
      <ul className="photo-grid">
        {detail.loading && !detail.data
          ? Array.from({ length: Math.min(cluster.size, 12) }, (_, i) => (
              <li key={i}>
                <div className="thumb thumb-loading" />
              </li>
            ))
          : members.slice(0, shown).map((m) => {
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
        <div className="list-more">
          <Button onClick={() => setShown((n) => n + PHOTOS_PER_PAGE)}>
            Show more · {members.length - shown} left
          </Button>
        </div>
      )}
    </>
  );
}

/** Photos, organised automatically. Nothing here was deleted: duplicates are
 *  grouped with the sharpest copy marked "best". */
export default function Photos() {
  const [kind, setKind] = useState<ClusterKind>("album");
  const [open, setOpen] = useState<ClusterSummary | null>(null);
  const groups = usePagedClusters(kind);
  const current = KINDS.find((k) => k.value === kind);

  if (open) {
    return (
      <>
        <Toolbar title={open.label} icon={Images} count={open.size}>
          <IconButton icon={ArrowLeft} label="All groups" size="sm" onClick={() => setOpen(null)} />
        </Toolbar>
        <div className="page">
          <div className="page-inner gallery">
            <GroupPhotos cluster={open} />
          </div>
        </div>
      </>
    );
  }

  return (
    <>
      <Toolbar title="Photos" icon={Images} count={groups.items.length || undefined}>
        <Segmented label="Photo grouping" options={KINDS} value={kind} onChange={setKind} />
      </Toolbar>
      {!groups.loading && !groups.error && groups.items.length === 0 ? (
        <EmptyState icon={Images} title={`No ${current?.label.toLowerCase() ?? "groups"} yet`}>
          {current?.empty}
        </EmptyState>
      ) : (
        <div className="page">
          <div className="page-inner gallery">
            {groups.error && <Callout title="Couldn't load photos">{groups.error}</Callout>}
            <ul className="album-grid">
              {groups.loading && groups.items.length === 0
                ? Array.from({ length: 6 }, (_, i) => (
                    <li key={i}>
                      <div className="album-cover">
                        <div className="thumb thumb-loading" />
                      </div>
                    </li>
                  ))
                : groups.items.map((c, i) => (
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
                          <span className="album-shade" aria-hidden />
                          <span className="album-caption">
                            <span className="album-label">{c.label}</span>
                            <span className="album-count">{plural(c.size, "photo")}</span>
                          </span>
                        </span>
                      </button>
                    </li>
                  ))}
            </ul>
            {groups.hasMore && (
              <div className="list-more">
                <Button onClick={groups.loadMore} loading={groups.loading}>
                  Load more
                </Button>
              </div>
            )}
          </div>
        </div>
      )}
    </>
  );
}
