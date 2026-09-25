import { useEffect, useState } from "react";
import {
  CircleCheck,
  Cpu,
  Download,
  ExternalLink,
  Monitor,
  Moon,
  Palette as PaletteIcon,
  RefreshCw,
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
import { Badge, Button, Callout, PageHeader, Switch } from "./ui";

const RELEASES_URL = "https://github.com/joeydd032995-pixel/Gather/releases";

const THEMES: { value: ThemeChoice; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

function SectionHead({
  icon: Icon,
  title,
  children,
}: {
  icon: typeof Sun;
  title: string;
  children?: React.ReactNode;
}) {
  return (
    <div className="settings-head">
      <span className="settings-icon" aria-hidden>
        <Icon />
      </span>
      <div>
        <h2 className="card-title">{title}</h2>
        {children && <p className="card-desc">{children}</p>}
      </div>
    </div>
  );
}

function Appearance({ theme, onTheme }: { theme: ThemeChoice; onTheme: (t: ThemeChoice) => void }) {
  return (
    <section className="card card-pad settings-section">
      <SectionHead icon={PaletteIcon} title="Appearance">
        Match your system, or pick light or dark.
      </SectionHead>
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
    </section>
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
      <PageHeader title="Settings" />
      <div className="settings">
        <Appearance theme={theme} onTheme={onTheme} />

        {isTauri ? (
          <>
            <section className="card card-pad settings-section">
              <SectionHead icon={RefreshCw} title="Updates">
                Gather works entirely offline. Checking for updates is the only thing that contacts
                the internet: one request to the project's release page, sending nothing about you
                or your data. It never happens unless you turn it on or press the button.
              </SectionHead>
              <div className="settings-rows">
                <Switch
                  checked={checkOnStart}
                  onChange={toggle}
                  label="Check for updates when Gather starts"
                />
                <div className="settings-row">
                  <Button
                    onClick={check}
                    icon={RefreshCw}
                    loading={busy === "check"}
                    disabled={busy !== null}
                  >
                    Check now
                  </Button>
                </div>
                {error && <Callout>{error}</Callout>}
                {result && <UpdateResult result={result} busy={busy} onInstall={install} />}
              </div>
            </section>
            <MemorySection />
          </>
        ) : (
          <Callout tone="neutral" icon={Monitor}>
            Updates and memory settings are available in the desktop app.
          </Callout>
        )}
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
    <section className="card card-pad settings-section">
      <SectionHead icon={Cpu} title="Memory">
        {info.profile === "low" ? "Low-memory mode" : "Standard memory mode"}
        {why && ` — ${why}.`}
      </SectionHead>
      <div className="settings-rows">
        <div className="settings-row">
          <Badge tone={info.profile === "low" ? "warning" : "accent"}>
            {info.profile === "low" ? "Low memory" : "Standard"}
          </Badge>
          {ram && <span className="hint num">{ram} RAM</span>}
        </div>
        {info.profile === "low" && (
          <p className="settings-text">
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
