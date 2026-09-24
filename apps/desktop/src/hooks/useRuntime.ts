import { useEffect, useState } from "react";
import { isTauri, runtimeStatus, type RuntimeStatus } from "../native";

const POLL_MS = 500;

/**
 * Follows the bundled database and daemon while they start. Settles on
 * "ready", "unmanaged" (dev build, or a daemon that was already running) or
 * "failed"; in a plain browser there is nothing to manage.
 */
export function useRuntime(): RuntimeStatus {
  const [status, setStatus] = useState<RuntimeStatus>(
    isTauri ? { state: "starting", step: "Starting" } : { state: "unmanaged" },
  );

  useEffect(() => {
    if (!isTauri) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const next = await runtimeStatus();
        if (cancelled) return;
        setStatus(next);
        if (next.state === "starting") timer = setTimeout(poll, POLL_MS);
      } catch (e) {
        if (!cancelled) {
          setStatus({ state: "failed", message: String(e), log_dir: "" });
        }
      }
    };
    poll();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, []);

  return status;
}
