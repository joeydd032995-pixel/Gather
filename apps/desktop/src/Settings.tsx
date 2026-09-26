import { useEffect, useState } from "react";
import {
  CircleCheck,
  Cpu,
  Download,
  ExternalLink,
  Keyboard,
  Monitor,
  Moon,
  Palette as PaletteIcon,
  RefreshCw,
  Settings as SettingsIcon,
  ShieldCheck,
  Sun,
} from "lucide-react";
import type { ThemeChoice } from "./hooks/useTheme";
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
import { Badge, Button, Callout, Kbd, Panel, Switch, Toolbar } from "./ui";

const RELEASES_URL = "https://github.com/joeydd032995-pixel/Gather/releases";
const MOD = /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘" : "Ctrl";

const THEMES: { value: ThemeChoice; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

const SHORTCUTS: [string[], string][] = [
  [[MOD, "K"], "Jump to a view or run a command"],
  [[MOD, "O"], "Add files"],
  [["/"], "Search the Library, or find in the graph"],
  [["J", "K"], "Move through a list"],
  [["A", "R", "E", "D"], "Review: keep, remove, edit, dismiss"],
  [["U"], "Undo the last review answer"],
  [["0"], "Graph: fit everything in view"],
];

function Appearance({ theme, onTheme }: { theme: ThemeChoice; onTheme: (t: ThemeChoice) => void }) {
  return (
    <Panel title="Appearance" icon={PaletteIcon}>
      <div className="theme-picker" role="radiogroup" aria-label="Appearance">
        {THEMES.map(({ value, label, icon: Icon }) => (
          <button
            key={value}
            type="button"
            role="radio"
            aria-checked={theme === value}
            className={theme === value ? "theme-option active" : "theme-option"}
            onClick={() => onTheme(value)}
          >
            <span className="theme-preview-frame" aria-hidden>
              {(value === "system" ? ["light", "dark"] : [value]).map((v) => (
                <span key={v} className={`theme-preview theme-preview-${v}`}>
                  <span className="tp-side" />
                  <span className="tp-main">
                    <span className="tp-line" />
                    <span className="tp-line short" />
                    <span className="tp-card" />
                  </span>
                </span>
              ))}
            </span>
            <span className="theme-option-label">
              <Icon aria-hidden />
              {label}
            </span>
          </button>
        ))}
      </div>
    </Panel>
  );
}

/** App settings: appearance, the opt-in update check (the only feature that goes online) and the memory profile. */
export default function Settings({
  theme,
  onTheme,
}: {
  theme: ThemeChoice;
  onTheme: (t: ThemeChoice) => void;
}) {
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
      <Toolbar title="Settings" icon={SettingsIcon} />
      <div className="page">
        <div className="page-inner page-narrow settings">
          <Appearance theme={theme} onTheme={onTheme} />

          <Panel title="Privacy" icon={ShieldCheck}>
            <p className="panel-pad setting-desc-lg">
              Gather runs entirely on this computer. It has no account, no cloud and no telemetry;
              its services listen only on this machine. The update check below is the one feature
              that can go online, and it stays off unless you turn it on.
            </p>
          </Panel>

          {isTauri ? (
            <>
              <Panel title="Updates" icon={RefreshCw}>
                <Switch
                  checked={checkOnStart}
                  onChange={toggle}
                  label="Check for updates when Gather starts"
                  description="One request to the project's release page, sending nothing about you or your data."
                />
                <div className="setting-row">
                  <div className="setting-text">
                    <span className="setting-label">Check now</span>
                    <p className="setting-desc">Look for a newer version once.</p>
                  </div>
                  <Button
                    onClick={check}
                    icon={RefreshCw}
                    loading={busy === "check"}
                    disabled={busy !== null}
                  >
                    Check now
                  </Button>
                </div>
                {(error || result) && (
                  <div className="panel-pad">
                    {error && <Callout>{error}</Callout>}
                    {result && <UpdateResult result={result} busy={busy} onInstall={install} />}
                  </div>
                )}
              </Panel>
              <MemorySection />
            </>
          ) : (
            <Callout tone="neutral" icon={Monitor}>
              Updates and memory settings are available in the desktop app.
            </Callout>
          )}

          <Panel title="Keyboard" icon={Keyboard}>
            <ul className="shortcuts">
              {SHORTCUTS.map(([keys, what]) => (
                <li key={what}>
                  <span>{what}</span>
                  <span className="shortcut-keys">
                    {keys.map((k) => (
                      <Kbd key={k}>{k}</Kbd>
                    ))}
                  </span>
                </li>
              ))}
            </ul>
          </Panel>
        </div>
      </div>
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
    <Panel
      title="Memory"
      icon={Cpu}
      actions={
        <Badge tone={info.profile === "low" ? "warning" : "accent"}>
          {info.profile === "low" ? "Low memory" : "Standard"}
        </Badge>
      }
    >
      <div className="panel-pad">
        <p className="setting-desc-lg">
          {info.profile === "low" ? "Low-memory mode" : "Standard memory mode"}
          {why && ` — ${why}.`}
        </p>
        {info.profile === "low" && (
          <p className="setting-desc-lg">
            Gather keeps its database and background work small so it runs alongside your other
            apps. By default, files are limited to 32 MB each, and if you use a local AI model
            through Ollama, Gather uses it only for search (embeddings), one request at a time.
          </p>
        )}
        <p className="hint">
          To choose the mode yourself, start Gather with <code>GATHER_MEMORY_PROFILE</code> set to
          “low” or “standard”.
        </p>
      </div>
    </Panel>
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
      <Callout tone="neutral" icon={ExternalLink}>
        This build can't update itself. New versions are published at <code>{RELEASES_URL}</code>.
      </Callout>
    );
  }
  if (!result.available) {
    return (
      <Callout tone="success" icon={CircleCheck}>
        You're up to date (version {result.current_version}).
      </Callout>
    );
  }
  return (
    <Callout
      tone="accent"
      icon={Download}
      title={`Version ${result.version} is available`}
      action={
        <Button
          variant="primary"
          onClick={onInstall}
          loading={busy === "install"}
          disabled={busy !== null}
        >
          Install and restart
        </Button>
      }
    >
      You have {result.current_version}.
      {result.notes && <p className="update-notes">{result.notes}</p>}
    </Callout>
  );
}
