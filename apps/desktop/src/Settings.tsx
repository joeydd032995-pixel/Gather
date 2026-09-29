import { useEffect, useId, useState } from "react";
import {
  CircleCheck,
  Cpu,
  Download,
  ExternalLink,
  FileText,
  FolderOpen,
  Keyboard,
  Monitor,
  Moon,
  Palette as PaletteIcon,
  PlugZap,
  RefreshCw,
  Settings as SettingsIcon,
  ShieldCheck,
  Sparkles,
  Sun,
} from "lucide-react";
import { getStatus, type DaemonStatus } from "./api";
import type { ThemeChoice } from "./hooks/useTheme";
import {
  checkForUpdate,
  getAiSettings,
  getUpdateSettings,
  installUpdate,
  isTauri,
  logsDir,
  memoryProfile,
  openLogsFolder,
  saveAiSettings,
  setUpdateSettings,
  testOllama,
  type AiSettings,
  type AiSettingsView,
  type MemoryInfo,
  type ReadingSpeed,
  type UpdateCheck,
} from "./native";
import { Badge, Button, Callout, Kbd, Panel, Segmented, Switch, Toolbar, errorText } from "./ui";

const SPEEDS: { value: ReadingSpeed; label: string }[] = [
  { value: "gentle", label: "Gentle" },
  { value: "balanced", label: "Balanced" },
  { value: "full", label: "Full speed" },
];
const SPEED_HELP: Record<ReadingSpeed, string> = {
  gentle:
    "Works about a third of the time and rests the rest, so the computer stays usable. A big import takes longer.",
  balanced: "Works about 60% of the time.",
  full: "Never rests: the fastest, but the model can use nearly all of the processor for as long as there is something to read.",
};

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
              that can go online, and it stays off unless you turn it on. A local AI model, if you
              set one up, runs on this computer too.
            </p>
          </Panel>

          {isTauri ? (
            <>
              <AiSection />
              <ReadingSection />
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
              AI model, update and memory settings are available in the desktop app.
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

/** Suggested models, small enough for a 4 GB computer. */
const SUGGESTED_CHAT = "llama3.2:1b";
const SUGGESTED_EMBED = "nomic-embed-text";

/** Whether Ollama lists `name` ("x" is listed as "x:latest"). */
function hasModel(models: string[], name: string): boolean {
  return models.some((m) => m === name || m === `${name}:latest`);
}

type Test =
  | { state: "idle" | "testing" }
  | { state: "ok"; models: string[] }
  | { state: "error"; message: string };

/** The local AI model: where Ollama is, and which models Gather uses. */
function AiSection() {
  const [view, setView] = useState<AiSettingsView | null>(null);
  const [draft, setDraft] = useState<AiSettings | null>(null);
  const [test, setTest] = useState<Test>({ state: "idle" });
  const [running, setRunning] = useState<DaemonStatus["ai"] | null>(null);
  const [lowMemory, setLowMemory] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const urlId = useId();
  const chatId = useId();
  const embedId = useId();
  const listId = useId();

  useEffect(() => {
    getAiSettings()
      .then((v) => {
        setView(v);
        setDraft(v);
      })
      .catch((e) => setError(errorText(e)));
    getStatus()
      .then((s) => setRunning(s.ai))
      .catch(() => setRunning(null));
    memoryProfile()
      .then((m) => setLowMemory(m.profile === "low"))
      .catch(() => {});
  }, []);

  if (!draft || !view) {
    return error ? <Callout>{error}</Callout> : null;
  }

  const set = (patch: Partial<AiSettings>) => setDraft({ ...draft, ...patch });
  const dirty =
    draft.enabled !== view.enabled ||
    draft.url.trim() !== view.url ||
    draft.chat_model.trim() !== view.chat_model ||
    draft.embed_model.trim() !== view.embed_model ||
    draft.speed !== view.speed;

  const runTest = async () => {
    setTest({ state: "testing" });
    try {
      const { models } = await testOllama(draft.url.trim());
      setTest({ state: "ok", models });
    } catch (e) {
      setTest({ state: "error", message: errorText(e) });
    }
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      // Gather restarts to apply this: the start-up screen shows until it's back.
      await saveAiSettings(draft);
    } catch (e) {
      setError(errorText(e));
      setSaving(false);
    }
  };

  const models = test.state === "ok" ? test.models : null;
  const missing = (name: string) =>
    models !== null && name.trim() !== "" && !hasModel(models, name.trim());

  return (
    <Panel
      title="AI model"
      icon={Sparkles}
      actions={
        running?.enabled ? <Badge tone="success">On</Badge> : <Badge tone="neutral">Off</Badge>
      }
    >
      <Switch
        checked={draft.enabled}
        onChange={(enabled) => set({ enabled })}
        label="Use a local AI model (Ollama)"
        description="Search by meaning, and read files more thoroughly than Gather's built-in rules. Ollama runs on this computer; nothing is sent anywhere."
      />
      {draft.enabled && (
        <>
          <div className="setting-row setting-field">
            <div className="setting-text">
              <label htmlFor={urlId} className="setting-label">
                Ollama address
              </label>
              <p className="setting-desc">
                Install Ollama from ollama.com and start it. It listens on http://127.0.0.1:11434
                unless you changed that.
              </p>
            </div>
            <div className="setting-control">
              <input
                id={urlId}
                className="input"
                value={draft.url}
                spellCheck={false}
                onChange={(e) => {
                  set({ url: e.target.value });
                  setTest({ state: "idle" });
                }}
              />
              <Button icon={PlugZap} onClick={runTest} loading={test.state === "testing"}>
                Test
              </Button>
            </div>
          </div>
          {test.state === "ok" && (
            <div className="panel-pad">
              <Callout tone="success" icon={CircleCheck} title="Connected to Ollama">
                {test.models.length === 0
                  ? "It has no models yet: download them with the commands below."
                  : `Models on this computer: ${test.models.join(", ")}.`}
              </Callout>
            </div>
          )}
          {test.state === "error" && (
            <div className="panel-pad">
              <Callout title="Couldn't reach Ollama">{test.message}</Callout>
            </div>
          )}

          <datalist id={listId}>
            {(models ?? []).map((m) => (
              <option key={m} value={m} />
            ))}
          </datalist>
          <div className="setting-row setting-field">
            <div className="setting-text">
              <label htmlFor={chatId} className="setting-label">
                Model for reading files
              </label>
              <p className="setting-desc">
                Pulls facts and relationships out of what you add. On a computer with about 4 GB of
                memory, {SUGGESTED_CHAT} fits. Leave empty to use Ollama for search only.
              </p>
              {missing(draft.chat_model) && (
                <p className="setting-desc setting-warn">
                  Not downloaded yet: run <code>ollama pull {draft.chat_model.trim()}</code>
                </p>
              )}
            </div>
            <div className="setting-control">
              <input
                id={chatId}
                className="input"
                list={listId}
                value={draft.chat_model}
                placeholder={`None (e.g. ${SUGGESTED_CHAT})`}
                spellCheck={false}
                onChange={(e) => set({ chat_model: e.target.value })}
              />
            </div>
          </div>
          {draft.chat_model.trim() !== "" && (
            <div className="setting-row setting-field">
              <div className="setting-text">
                <span className="setting-label">Reading speed</span>
                <p className="setting-desc">
                  How hard the model works while it reads files. It uses every processor core it can
                  get, so a big import can keep the computer busy for hours.{" "}
                  {SPEED_HELP[draft.speed]}
                </p>
              </div>
              <div className="setting-control">
                <Segmented
                  label="Reading speed"
                  options={SPEEDS}
                  value={draft.speed}
                  onChange={(speed) => set({ speed })}
                />
              </div>
            </div>
          )}
          <div className="setting-row setting-field">
            <div className="setting-text">
              <label htmlFor={embedId} className="setting-label">
                Model for search
              </label>
              <p className="setting-desc">
                Finds things by meaning, not just matching words. {SUGGESTED_EMBED} is small and is
                the one Gather is built for.
              </p>
              {missing(draft.embed_model) && (
                <p className="setting-desc setting-warn">
                  Not downloaded yet: run <code>ollama pull {draft.embed_model.trim()}</code>
                </p>
              )}
            </div>
            <div className="setting-control">
              <input
                id={embedId}
                className="input"
                list={listId}
                value={draft.embed_model}
                placeholder={SUGGESTED_EMBED}
                spellCheck={false}
                onChange={(e) => set({ embed_model: e.target.value })}
              />
            </div>
          </div>
          {lowMemory && draft.chat_model.trim() !== "" && (
            <div className="panel-pad">
              <Callout tone="warning" title="This computer has little memory">
                A model that reads files uses about 1.5 GB while it works, and reading goes slower.
                Gather unloads it a minute after it's done. If things get sluggish, clear the model
                for reading files and keep search.
              </Callout>
            </div>
          )}
        </>
      )}

      <div className="setting-row">
        <div className="setting-text">
          <span className="setting-label">Now running</span>
          <p className="setting-desc">
            {running === null
              ? "Gather's background service isn't answering."
              : running.enabled
                ? running.model
                  ? `Reading files with ${running.model}, search with ${running.embed_model}.`
                  : `Search with ${running.embed_model}; files are read with the built-in rules.`
                : "No AI model: files are read with the built-in rules, and search matches words."}
            {view.source === "environment" &&
              " (Set up by GATHER_OLLAMA_* variables; saving here takes over from them.)"}
          </p>
        </div>
        <Button variant="primary" onClick={save} loading={saving} disabled={!dirty}>
          Save and restart
        </Button>
      </div>
      {error && (
        <div className="panel-pad">
          <Callout>{error}</Callout>
        </div>
      )}
      <p className="hint panel-pad">
        Saving checks the search model with Ollama first, so keep Ollama running. Files already in
        Gather keep what was found in them; the new model reads files you add from now on.
      </p>
    </Panel>
  );
}

/** How far reading has got, and where the logs are. */
function ReadingSection() {
  const [status, setStatus] = useState<DaemonStatus["reading"] | null>(null);
  const [dir, setDir] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let cancelled = false;
    const poll = () =>
      getStatus()
        .then((s) => !cancelled && setStatus(s.reading))
        .catch(() => !cancelled && setStatus(null))
        .finally(() => {
          if (!cancelled) timer = setTimeout(poll, 10_000);
        });
    poll();
    logsDir()
      .then(setDir)
      .catch(() => setDir(null));
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, []);

  const open = async () => {
    setError(null);
    try {
      await openLogsFolder();
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <Panel title="Reading and logs" icon={FileText}>
      <div className="setting-row">
        <div className="setting-text">
          <span className="setting-label">Reading</span>
          <p className="setting-desc">
            {status === null
              ? "Gather's background service isn't answering."
              : status.chunks === 0 && status.files === 0
                ? "Everything you've added has been read."
                : `Reading ${status.files.toLocaleString()} ${status.files === 1 ? "file" : "files"}: ${status.chunks.toLocaleString()} ${status.chunks === 1 ? "section" : "sections"} to go. Files finish one at a time, oldest first.`}
            {status !== null &&
              status.failed > 0 &&
              ` ${status.failed.toLocaleString()} ${status.failed === 1 ? "section" : "sections"} couldn't be read and ${status.failed === 1 ? "was" : "were"} skipped; daemon.log says why.`}
          </p>
        </div>
      </div>
      <div className="setting-row">
        <div className="setting-text">
          <span className="setting-label">Logs</span>
          <p className="setting-desc">
            What Gather's background service did, including any errors: daemon.log (and postgres.log
            for the database).
            {dir && (
              <>
                {" "}
                <code className="setting-path">{dir}</code>
              </>
            )}
          </p>
        </div>
        <Button icon={FolderOpen} onClick={open}>
          Open folder
        </Button>
      </div>
      {error && (
        <div className="panel-pad">
          <Callout>{error}</Callout>
        </div>
      )}
    </Panel>
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
