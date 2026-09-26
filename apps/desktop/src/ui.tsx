// Shared UI primitives. Views compose these rather than styling their own
// buttons, rows and panels, so the whole app reads as one system.

import {
  createContext,
  useContext,
  useId,
  useRef,
  type ButtonHTMLAttributes,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { CircleAlert, LoaderCircle, type LucideIcon } from "lucide-react";

type Variant = "primary" | "secondary" | "ghost" | "danger" | "subtle";
type Size = "sm" | "md" | "lg";

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
  icon?: LucideIcon;
  /** Shows a spinner in place of the icon and disables the button. */
  loading?: boolean;
  /** A keyboard shortcut shown at the trailing edge. */
  shortcut?: string;
}

export function Button({
  variant = "secondary",
  size = "md",
  icon: Icon,
  loading = false,
  shortcut,
  className,
  children,
  disabled,
  type = "button",
  ...rest
}: ButtonProps) {
  return (
    <button
      type={type}
      className={`btn btn-${variant} btn-${size}${className ? ` ${className}` : ""}`}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...rest}
    >
      {loading ? (
        <LoaderCircle className="btn-icon spin" aria-hidden />
      ) : (
        Icon && <Icon className="btn-icon" aria-hidden />
      )}
      {children && <span className="btn-label">{children}</span>}
      {shortcut && (
        <kbd className="btn-kbd" aria-hidden>
          {shortcut}
        </kbd>
      )}
    </button>
  );
}

interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  icon: LucideIcon;
  /** Accessible name; also the tooltip. */
  label: string;
  variant?: Variant;
  size?: Size;
  /** Where the tooltip opens. */
  tip?: "top" | "bottom" | "left" | "right";
}

export function IconButton({
  icon: Icon,
  label,
  variant = "ghost",
  size = "md",
  tip = "bottom",
  className,
  type = "button",
  ...rest
}: IconButtonProps) {
  return (
    <button
      type={type}
      aria-label={label}
      data-tip={label}
      data-tip-side={tip}
      className={`btn btn-${variant} btn-${size} btn-icon-only${className ? ` ${className}` : ""}`}
      {...rest}
    >
      <Icon className="btn-icon" aria-hidden />
    </button>
  );
}

export type Tone = "neutral" | "accent" | "success" | "warning" | "danger" | "info";

export function Badge({
  tone = "neutral",
  icon: Icon,
  children,
  title,
  className,
}: {
  tone?: Tone;
  icon?: LucideIcon;
  children: ReactNode;
  title?: string;
  className?: string;
}) {
  return (
    <span className={`badge badge-${tone}${className ? ` ${className}` : ""}`} title={title}>
      {Icon && <Icon className="badge-icon" aria-hidden />}
      {children}
    </span>
  );
}

/** A unit or entity kind: a coloured dot and its name. */
export function KindTag({ kind, label }: { kind: string; label?: string }) {
  return (
    <span className="kind-tag" data-kind={kind}>
      <span className="kind-dot" aria-hidden />
      {label ?? kind}
    </span>
  );
}

export function Kbd({ children, className }: { children: ReactNode; className?: string }) {
  return <kbd className={`kbd${className ? ` ${className}` : ""}`}>{children}</kbd>;
}

export function Spinner({ label = "Loading" }: { label?: string }) {
  return (
    <span className="spinner" role="status">
      <LoaderCircle className="spin" aria-hidden />
      <span className="visually-hidden">{label}</span>
    </span>
  );
}

/** Placeholder rows in the shape of the content that is loading. */
export function Skeleton({
  rows = 4,
  variant = "row",
}: {
  rows?: number;
  variant?: "row" | "card";
}) {
  return (
    <div className={`skeleton skeleton-${variant}`} role="status" aria-label="Loading">
      {Array.from({ length: rows }, (_, i) => (
        <div className="skeleton-item" key={i} style={{ animationDelay: `${i * 60}ms` }}>
          <span className="skeleton-bar" style={{ width: `${86 - ((i * 17) % 38)}%` }} />
          <span
            className="skeleton-bar skeleton-bar-sub"
            style={{ width: `${38 + ((i * 23) % 30)}%` }}
          />
        </div>
      ))}
    </div>
  );
}

export function EmptyState({
  icon: Icon,
  title,
  children,
  action,
  tone = "neutral",
  compact = false,
}: {
  icon: LucideIcon;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
  tone?: "neutral" | "success";
  compact?: boolean;
}) {
  return (
    <div className={`empty empty-${tone}${compact ? " empty-compact" : ""}`}>
      <div className="empty-art" aria-hidden>
        <span className="empty-ring r1" />
        <span className="empty-ring r2" />
        <span className="empty-icon">
          <Icon />
        </span>
      </div>
      <h2 className="empty-title">{title}</h2>
      {children && <div className="empty-body">{children}</div>}
      {action && <div className="empty-action">{action}</div>}
    </div>
  );
}

export function Callout({
  tone = "danger",
  icon: Icon = CircleAlert,
  title,
  children,
  action,
}: {
  tone?: Tone;
  icon?: LucideIcon;
  title?: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className={`callout callout-${tone}`} role={tone === "danger" ? "alert" : "status"}>
      <Icon className="callout-icon" aria-hidden />
      <div className="callout-body">
        {title && <p className="callout-title">{title}</p>}
        {children && <div className="callout-text">{children}</div>}
      </div>
      {action && <div className="callout-action">{action}</div>}
    </div>
  );
}

/** A 0–1 value as a short bar plus its number. */
export function Meter({
  value,
  label,
  tone = "accent",
  digits = 2,
  width = 48,
  showValue = true,
}: {
  value: number;
  label: string;
  tone?: Tone;
  digits?: number;
  width?: number;
  showValue?: boolean;
}) {
  const pct = Math.max(0, Math.min(1, value)) * 100;
  return (
    <span className={`meter meter-${tone}`} title={`${label}: ${value.toFixed(digits)}`}>
      <span
        className="meter-track"
        style={{ width }}
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={1}
        aria-valuenow={Number(value.toFixed(digits))}
      >
        <span className="meter-fill" style={{ width: `${pct}%` }} />
      </span>
      {showValue && <span className="meter-value num">{value.toFixed(digits)}</span>}
    </span>
  );
}

/** A small set of mutually exclusive options (a radio group styled as tabs). */
export function Segmented<T extends string>({
  options,
  value,
  onChange,
  label,
}: {
  options: { value: T; label: string; count?: number }[];
  value: T;
  onChange: (value: T) => void;
  label: string;
}) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const onKey = (e: KeyboardEvent, index: number) => {
    const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const next = (index + step + options.length) % options.length;
    onChange(options[next].value);
    refs.current[next]?.focus();
  };
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map((o, i) => {
        const active = o.value === value;
        return (
          <button
            key={o.value}
            ref={(el) => {
              refs.current[i] = el;
            }}
            type="button"
            role="radio"
            aria-checked={active}
            tabIndex={active ? 0 : -1}
            className={active ? "segment active" : "segment"}
            onClick={() => onChange(o.value)}
            onKeyDown={(e) => onKey(e, i)}
          >
            {o.label}
            {o.count !== undefined && <span className="segment-count num">{o.count}</span>}
          </button>
        );
      })}
    </div>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  description,
  disabled,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label: string;
  description?: ReactNode;
  disabled?: boolean;
}) {
  const id = useId();
  return (
    <div className="setting-row">
      <div className="setting-text">
        <label htmlFor={id} className="setting-label">
          {label}
        </label>
        {description && (
          <p className="setting-desc" id={`${id}-desc`}>
            {description}
          </p>
        )}
      </div>
      <button
        id={id}
        type="button"
        role="switch"
        aria-checked={checked}
        aria-describedby={description ? `${id}-desc` : undefined}
        className="switch"
        disabled={disabled}
        onClick={() => onChange(!checked)}
      >
        <span className="switch-thumb" />
      </button>
    </div>
  );
}

/** The navigation group of the current view ("Explore", "Needs you"…), shown
 *  as a breadcrumb in every toolbar. */
export const SectionContext = createContext<string | null>(null);

/**
 * The strip across the top of every view: breadcrumb, title and a count on
 * the left, the view's own controls on the right.
 */
export function Toolbar({
  title,
  icon: Icon,
  count,
  children,
}: {
  title: string;
  icon?: LucideIcon;
  count?: ReactNode;
  children?: ReactNode;
}) {
  const section = useContext(SectionContext);
  return (
    <header className="toolbar">
      <div className="toolbar-title">
        {Icon && (
          <span className="toolbar-icon" aria-hidden>
            <Icon />
          </span>
        )}
        {section && (
          <>
            <span className="toolbar-crumb">{section}</span>
            <span className="toolbar-slash" aria-hidden>
              /
            </span>
          </>
        )}
        <h1 tabIndex={-1} data-page-title>
          {title}
        </h1>
        {count !== undefined && count !== null && (
          <span className="toolbar-count num">{count}</span>
        )}
      </div>
      {children && <div className="toolbar-actions">{children}</div>}
    </header>
  );
}

/** A list on the left, the selected item's detail on the right. */
export function SplitView({
  list,
  detail,
  listLabel,
  listHeader,
}: {
  list: ReactNode;
  detail: ReactNode;
  listLabel: string;
  listHeader?: ReactNode;
}) {
  return (
    <div className="split">
      <section className="split-list" aria-label={listLabel}>
        {listHeader && <div className="split-list-head">{listHeader}</div>}
        <div className="split-list-body">{list}</div>
      </section>
      <section className="split-detail" aria-live="polite">
        {detail}
      </section>
    </div>
  );
}

/** A titled group inside an inspector or settings page. */
export function Panel({
  title,
  icon: Icon,
  actions,
  children,
  className,
}: {
  title?: ReactNode;
  icon?: LucideIcon;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`panel${className ? ` ${className}` : ""}`}>
      {(title || actions) && (
        <header className="panel-head">
          {title && (
            <h2 className="panel-title">
              {Icon && <Icon aria-hidden />}
              {title}
            </h2>
          )}
          {actions && <div className="panel-actions">{actions}</div>}
        </header>
      )}
      <div className="panel-body">{children}</div>
    </section>
  );
}

/** A labelled number, for dashboards. */
export function StatTile({
  icon: Icon,
  label,
  value,
  hint,
  tone = "neutral",
  onClick,
}: {
  icon: LucideIcon;
  label: string;
  value: ReactNode;
  hint?: ReactNode;
  tone?: Tone;
  onClick?: () => void;
}) {
  const body = (
    <>
      <span className={`stat-icon stat-${tone}`} aria-hidden>
        <Icon />
      </span>
      <span className="stat-value num">{value}</span>
      <span className="stat-label">{label}</span>
      {hint && <span className="stat-hint">{hint}</span>}
    </>
  );
  return onClick ? (
    <button type="button" className="stat" onClick={onClick}>
      {body}
    </button>
  ) : (
    <div className="stat">{body}</div>
  );
}

/** The error message of anything thrown, in words a person can act on. */
export function errorText(e: unknown): string {
  const message = e instanceof Error ? e.message : String(e);
  // fetch() rejects with a bare TypeError when nothing answers on loopback.
  if (e instanceof TypeError && /fetch|network|load failed/i.test(message)) {
    return "Gather's local daemon isn't responding. It may still be starting; try again in a moment.";
  }
  return message;
}

const RELATIVE = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 86400],
  ["month", 30 * 86400],
  ["week", 7 * 86400],
  ["day", 86400],
  ["hour", 3600],
  ["minute", 60],
];

/** "3 days ago", "yesterday", "just now". */
export function relativeTime(iso: string): string {
  const seconds = (new Date(iso).getTime() - Date.now()) / 1000;
  for (const [unit, size] of UNITS) {
    if (Math.abs(seconds) >= size) return RELATIVE.format(Math.round(seconds / size), unit);
  }
  return "just now";
}

/** A time element with the relative time shown and the exact one on hover. */
export function When({ iso, className }: { iso: string; className?: string }) {
  return (
    <time className={className} dateTime={iso} title={new Date(iso).toLocaleString()}>
      {relativeTime(iso)}
    </time>
  );
}
