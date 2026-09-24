import { useEffect, useState } from "react";
import {
  checkForUpdate,
  getUpdateSettings,
  installUpdate,
  isTauri,
  setUpdateSettings,
  type UpdateCheck,
} from "./native";

const RELEASES_URL = "https://github.com/joeydd032995-pixel/Gather/releases";

/** App settings. Today: the opt-in update check, the only feature that goes online. */
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
    <section>
      <h2>Updates</h2>
      <p className="hint">
        Gather works entirely offline. Checking for updates is the only thing that
        contacts the internet: one request to the project's release page, sending nothing
        about you or your data. It never happens unless you turn it on or press the button.
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
        Version {result.version} is available (you have {result.current_version}).
      </p>
      {result.notes && <p className="hint">{result.notes}</p>}
      <button onClick={onInstall} disabled={busy !== null}>
        {busy === "install" ? "Installing…" : "Install and restart"}
      </button>
    </div>
  );
}
