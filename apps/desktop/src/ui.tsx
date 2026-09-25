// Shared UI primitives. Views compose these rather than styling their own
// buttons, badges and empty states, so the whole app reads as one system.

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
      {shortcut && <Kbd className="btn-kbd">{shortcut}</Kbd>}
    </button>
  );
}

interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  icon: LucideIcon;
  /** Accessible name; also the tooltip. */
  label: string;
  variant?: Variant;
  size?: Size;
}

export function IconButton({
  icon: Icon,
  label,
  variant = "ghost",
  size = "md",
  className,
  type = "button",
  ...rest
}: IconButtonProps) {
  return (
    <button
      type={type}
      aria-label={label}
      title={label}
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
          <span className="skeleton-bar" style={{ width: `${88 - ((i * 17) % 40)}%` }} />
          <span
            className="skeleton-bar skeleton-bar-sub"
            style={{ width: `${40 + ((i * 23) % 30)}%` }}
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
}: {
  icon: LucideIcon;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
  tone?: "neutral" | "success";
}) {
  return (
    <div className={`empty empty-${tone}`}>
      <div className="empty-icon" aria-hidden>
        <Icon />
      </div>
      <h3 className="empty-title">{title}</h3>
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
  width = 56,
}: {
  value: number;
  label: string;
  tone?: Tone;
  digits?: number;
  width?: number;
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
      <span className="meter-value num">{value.toFixed(digits)}</span>
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
    <div className="switch-row">
      <div className="switch-text">
        <label htmlFor={id} className="switch-label">
          {label}
        </label>
        {description && (
          <p className="switch-desc" id={`${id}-desc`}>
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

/** The navigation group of the current view ("Explore", "Needs you"…),
 *  shown above every page title so titles sit at the same height. */
export const SectionContext = createContext<string | null>(null);

export function PageHeader({
  title,
  description,
  actions,
  eyebrow,
}: {
  title: string;
  description?: ReactNode;
  actions?: ReactNode;
  eyebrow?: ReactNode;
}) {
  const section = useContext(SectionContext);
  return (
    <header className="page-header">
      <div className="page-heading">
        {(section || eyebrow) && (
          <div className="page-eyebrow">
            {section && <span>{section}</span>}
            {section && eyebrow && <span aria-hidden>·</span>}
            {eyebrow && <span className="page-eyebrow-detail">{eyebrow}</span>}
          </div>
        )}
        <h1 className="page-title" tabIndex={-1} data-page-title>
          {title}
        </h1>
        {description && <p className="page-desc">{description}</p>}
      </div>
      {actions && <div className="page-actions">{actions}</div>}
    </header>
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
