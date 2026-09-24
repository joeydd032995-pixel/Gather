import { useEffect, useState } from "react";
import { isTauri, runtimeStatus, type RuntimeStatus } from "../native";

const POLL_MS = 500;
/** Slower once settled: the stack can restart (e.g. after a failed update install). */
const SETTLED_POLL_MS = 3000;

/**
 * Follows the bundled database and daemon: "starting" while they come up,
 * then "ready", "unmanaged" (dev build, or a daemon the user runs) or
 * "failed". In a plain browser there is nothing to manage.
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
        timer = setTimeout(poll, next.state === "starting" ? POLL_MS : SETTLED_POLL_MS);
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
