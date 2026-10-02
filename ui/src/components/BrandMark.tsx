/** The Switchyard mark: one track in, a switch, three tracks out. */
export function BrandMark({ className = 'brand-mark' }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 28 28" aria-hidden focusable="false">
      <rect width="28" height="28" rx="7" fill="#17140e" />
      <rect x="0.5" y="0.5" width="27" height="27" rx="6.5" fill="none" stroke="#f2a93b" strokeOpacity="0.22" />
      <g fill="none" stroke="#f2a93b" strokeWidth="2.2" strokeLinecap="round">
        <path d="M6 14h4.5c4.2 0 5.6-6.5 12-6.5" strokeOpacity="0.55" />
        <path d="M10.5 14c4.2 0 5.6 6.5 12 6.5" strokeOpacity="0.55" />
        <path d="M6 14h16.5" />
      </g>
      <circle cx="6" cy="14" r="2.4" fill="#ffbb55" />
    </svg>
  );
}
