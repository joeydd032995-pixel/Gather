import { useEffect, useRef, useState } from "react";
import { ImageOff } from "lucide-react";
import { fetchThumbnailUrl } from "./api";

interface ThumbnailProps {
  imageId: string;
  alt: string;
  /** Square size in pixels; omitted, the thumbnail fills its container. */
  size?: number;
}

/** Start fetching a little before the thumbnail scrolls into view. */
const PRELOAD_MARGIN = "200px";

/**
 * A locally rendered photo thumbnail, fetched with the API token. Each one
 * costs the daemon a full image decode, so it is only requested once the
 * placeholder is (nearly) on screen.
 */
export default function Thumbnail({ imageId, alt, size }: ThumbnailProps) {
  const placeholder = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    const node = placeholder.current;
    if (!node || visible) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) setVisible(true);
      },
      { rootMargin: PRELOAD_MARGIN },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [visible]);

  useEffect(() => {
    if (!visible) return;
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
  }, [imageId, visible]);

  const style = size ? { width: size, height: size } : undefined;
  if (failed) {
    return (
      <div className="thumb thumb-missing" style={style} title="Preview unavailable">
        <ImageOff aria-hidden />
        <span className="visually-hidden">{alt}: preview unavailable</span>
      </div>
    );
  }
  if (!url) return <div ref={placeholder} className="thumb thumb-loading" style={style} />;
  return <img className="thumb thumb-ready" style={style} src={url} alt={alt} decoding="async" />;
}
