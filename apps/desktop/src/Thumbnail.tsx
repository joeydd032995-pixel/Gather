import { useEffect, useState } from "react";
import { fetchThumbnailUrl } from "./api";

interface ThumbnailProps {
  imageId: string;
  alt: string;
  size?: number;
}

/** A locally rendered photo thumbnail, fetched with the API token. */
export default function Thumbnail({ imageId, alt, size = 128 }: ThumbnailProps) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    let objectUrl: string | null = null;
    fetchThumbnailUrl(imageId)
      .then((u) => {
        objectUrl = u;
        if (cancelled) {
          URL.revokeObjectURL(u);
        } else {
          setUrl(u);
        }
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [imageId]);

  const style = { width: size, height: size };
  if (failed) {
    return (
      <div className="thumb thumb-missing" style={style} title="preview unavailable">
        ?
      </div>
    );
  }
  if (!url) return <div className="thumb" style={style} />;
  return <img className="thumb" style={style} src={url} alt={alt} />;
}
