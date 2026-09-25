import { useEffect, useState } from "react";
import {
  checkForUpdate,
  getUpdateSettings,
  installUpdate,
  isTauri,
  memoryProfile,
  setUpdateSettings,
  type MemoryInfo,
  type UpdateCheck,
} from "./native";

const RELEASES_URL = "https://github.com/joeydd032995-pixel/Gather/releases";

/** App settings: the opt-in update check (the only feature that goes online) and the memory profile. */
export default function Settings() {
  const [checkOnStart, setCheckOnStart] = useState(false);
  const [result, setResult] = useState<UpdateCheck | null>(null);
  const [busy, setBusy] = useState<"check" | "install" | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isTauri) return;
    getUpdateSettings()
      .then((s) => setCheckOnStart(s.check_on_start))
      .catch((e) => setError(String(e)));
  }, []);

  if (!isTauri) {
    return <p className="prov-empty">Settings are available in the desktop app.</p>;
  }

  const toggle = async (enabled: boolean) => {
    setError(null);
    try {
      await setUpdateSettings({ check_on_start: enabled });
      setCheckOnStart(enabled);
    } catch (e) {
      setError(String(e));
    }
  };

  const check = async () => {
    setBusy("check");
    setError(null);
    try {
      setResult(await checkForUpdate());
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  };

  const install = async () => {
    setBusy("install");
    setError(null);
    try {
      await installUpdate(); // restarts the app on success
    } catch (e) {
      setError(String(e));
      setBusy(null);
    }
  };

  return (
    <>
      <section>
        <h2>Updates</h2>
        <p className="hint">
          Gather works entirely offline. Checking for updates is the only thing that contacts the
          internet: one request to the project's release page, sending nothing about you or your
          data. It never happens unless you turn it on or press the button.
        </p>
        <label>
          <input
            type="checkbox"
            checked={checkOnStart}
            onChange={(e) => toggle(e.target.checked)}
          />{" "}
          Check for updates when Gather starts
        </label>
        <p>
          <button onClick={check} disabled={busy !== null}>
            {busy === "check" ? "Checking…" : "Check now"}
          </button>
        </p>
        {error && <p className="error">{error}</p>}
        {result && <UpdateResult result={result} busy={busy} onInstall={install} />}
      </section>
      <MemorySection />
    </>
  );
}

function MemorySection() {
  const [info, setInfo] = useState<MemoryInfo | null>(null);

  useEffect(() => {
    memoryProfile()
      .then(setInfo)
      .catch(() => setInfo(null));
  }, []);

  if (!info) return null;
  const ram = info.total_mb === null ? null : `${(info.total_mb / 1024).toFixed(1)} GB`;
  const why = info.overridden
    ? "chosen by the GATHER_MEMORY_PROFILE setting"
    : ram && `this computer has ${ram} of memory`;
  return (
    <section>
      <h2>Memory</h2>
      {info.profile === "low" ? (
        <p>
          Low-memory mode is on{why && ` (${why})`}. Gather keeps its database and background work
          small so it runs alongside your other apps. By default, files are limited to 32 MB each,
          and if you use a local AI model through Ollama, Gather uses it only for search
          (embeddings), one request at a time.
        </p>
      ) : (
        <p>Standard memory mode{why && ` (${why})`}.</p>
      )}
      <p className="hint">
        To choose the mode yourself, start Gather with GATHER_MEMORY_PROFILE set to "low" or
        "standard".
      </p>
    </section>
  );
}

function UpdateResult({
  result,
  busy,
  onInstall,
}: {
  result: UpdateCheck;
  busy: "check" | "install" | null;
  onInstall: () => void;
}) {
  if (!result.supported) {
    return (
      <p className="hint">
        This build can't update itself. New versions are published at {RELEASES_URL}.
      </p>
    );
  }
  if (!result.available) {
    return <p className="all-clear">You're up to date (version {result.current_version}).</p>;
  }
  return (
    <div>
      <p>
        Version {result.version} is available (you have {result.current_version}
        ).
      </p>
      {result.notes && <p className="hint">{result.notes}</p>}
      <button onClick={onInstall} disabled={busy !== null}>
        {busy === "install" ? "Installing…" : "Install and restart"}
      </button>
    </div>
  );
}
