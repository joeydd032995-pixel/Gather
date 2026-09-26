import { useEffect, useId, useMemo, useRef, useState } from "react";
import { ArrowUpDown, CornerDownLeft, Search, type LucideIcon } from "lucide-react";
import { Kbd } from "./ui";

export interface Command {
  id: string;
  group: string;
  label: string;
  hint?: string;
  icon: LucideIcon;
  run: () => void;
}

/** Fuzzy enough: every typed word must start a word in the label or hint. */
function matches(command: Command, query: string): boolean {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const haystack = `${command.label} ${command.hint ?? ""} ${command.group}`.toLowerCase();
  return words.every((w) => haystack.includes(w));
}

/** Ctrl/⌘+K: jump anywhere or run an action, from the keyboard. */
export default function Palette({
  commands,
  onClose,
}: {
  commands: Command[];
  onClose: () => void;
}) {
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const id = useId();

  const shown = useMemo(
    () => (query.trim() ? commands.filter((c) => matches(c, query)) : commands),
    [commands, query],
  );

  useEffect(() => setActive(0), [query]);

  // Return focus to whatever had it once the palette closes.
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    input.current?.focus();
    return () => previous?.focus?.();
  }, []);

  useEffect(() => {
    list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  const run = (command: Command | undefined) => {
    if (!command) return;
    onClose();
    command.run();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((a) => Math.min(a + 1, shown.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => Math.max(a - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      run(shown[active]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "Tab") {
      // The dialog has one control; keep focus inside it.
      e.preventDefault();
    }
  };

  let lastGroup = "";
  return (
    <div className="dialog-backdrop" onMouseDown={onClose}>
      <div
        className="palette"
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <div className="palette-search">
          <Search aria-hidden />
          <input
            ref={input}
            role="combobox"
            aria-expanded="true"
            aria-controls={`${id}-list`}
            aria-activedescendant={shown[active] ? `${id}-${shown[active].id}` : undefined}
            aria-autocomplete="list"
            placeholder="Jump to a view or run a command…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            spellCheck={false}
          />
          <Kbd>Esc</Kbd>
        </div>
        <div
          className="palette-list"
          id={`${id}-list`}
          role="listbox"
          ref={list}
          aria-label="Commands"
        >
          {shown.length === 0 && <p className="palette-empty">No command matches “{query}”.</p>}
          {shown.map((c, i) => {
            const header = c.group !== lastGroup ? c.group : null;
            lastGroup = c.group;
            const Icon = c.icon;
            return (
              <div key={c.id} role="presentation">
                {header && (
                  <div className="palette-group" role="presentation">
                    {header}
                  </div>
                )}
                <div
                  id={`${id}-${c.id}`}
                  role="option"
                  aria-selected={i === active}
                  data-index={i}
                  className="palette-option"
                  onMouseMove={() => setActive(i)}
                  onClick={() => run(c)}
                >
                  <Icon aria-hidden />
                  <span>{c.label}</span>
                  {c.hint && <span className="palette-hint">{c.hint}</span>}
                </div>
              </div>
            );
          })}
        </div>
        <div className="palette-footer" aria-hidden>
          <span>
            <ArrowUpDown size={12} /> move
          </span>
          <span>
            <CornerDownLeft size={12} /> open
          </span>
        </div>
      </div>
    </div>
  );
}
