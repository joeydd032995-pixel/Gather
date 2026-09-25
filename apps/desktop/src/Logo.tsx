/** The Gather mark: scattered points drawn in toward a centre. */
export default function Logo({ size = 28 }: { size?: number }) {
  return (
    <svg className="logo" width={size} height={size} viewBox="0 0 32 32" aria-hidden>
      <rect width="32" height="32" rx="9" fill="var(--accent)" />
      <circle cx="16" cy="16" r="4.2" fill="var(--accent-fg)" />
      <circle
        cx="16"
        cy="16"
        r="8.6"
        fill="none"
        stroke="var(--accent-fg)"
        strokeOpacity="0.35"
        strokeWidth="1.2"
      />
      <circle cx="23.4" cy="11.6" r="1.9" fill="var(--accent-fg)" fillOpacity="0.9" />
      <circle cx="9.2" cy="21.4" r="1.6" fill="var(--accent-fg)" fillOpacity="0.75" />
      <circle cx="10.4" cy="9.6" r="1.25" fill="var(--accent-fg)" fillOpacity="0.6" />
      <circle cx="24.6" cy="22.8" r="1.05" fill="var(--accent-fg)" fillOpacity="0.5" />
    </svg>
  );
}
