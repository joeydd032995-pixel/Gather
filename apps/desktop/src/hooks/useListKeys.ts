import { useEffect, useRef } from "react";

function isTyping(target: EventTarget | null): boolean {
  return (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  );
}

/**
 * J/K (and ↑/↓) move the selection through a master list, and the selected
 * row is kept in view. Returns the ref to put on the list element.
 */
export function useListKeys<T>(
  items: T[],
  selected: string | null,
  keyOf: (item: T) => string,
  select: (key: string) => void,
) {
  const listRef = useRef<HTMLUListElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (isTyping(e.target) || e.metaKey || e.ctrlKey || e.altKey) return;
      if (document.querySelector('[role="dialog"]')) return;
      const key = e.key.toLowerCase();
      const step =
        key === "j" || key === "arrowdown" ? 1 : key === "k" || key === "arrowup" ? -1 : 0;
      if (!step || items.length === 0) return;
      e.preventDefault();
      const at = items.findIndex((i) => keyOf(i) === selected);
      const next = Math.min(items.length - 1, Math.max(0, (at < 0 ? -step : at) + step));
      select(keyOf(items[next]));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [items, keyOf, select, selected]);

  useEffect(() => {
    listRef.current
      ?.querySelector('[aria-current="true"]')
      ?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, [selected]);

  return listRef;
}
