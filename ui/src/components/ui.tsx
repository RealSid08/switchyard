import { CircleAlert, CircleCheck, Info, LoaderCircle, TriangleAlert, type LucideIcon } from 'lucide-react';
import { useId, useRef, type ComponentPropsWithRef, type CSSProperties, type KeyboardEvent, type ReactNode } from 'react';
import { outcome, statusCode } from '../lib/format';
import type { ConnectionKind, RequestRecord } from '../lib/types';

type ButtonVariant = 'default' | 'primary' | 'ghost' | 'danger' | 'danger-ghost';

interface ButtonProps extends ComponentPropsWithRef<'button'> {
  variant?: ButtonVariant;
  size?: 'sm' | 'md' | 'lg';
  icon?: LucideIcon;
  iconOnly?: boolean;
  loading?: boolean;
  block?: boolean;
}

export function Button({ variant = 'default', size = 'md', icon: Icon, iconOnly, loading, block, className, children, disabled, type = 'button', ...rest }: ButtonProps) {
  const cls = ['btn', variant !== 'default' && `btn-${variant}`, size !== 'md' && `btn-${size}`, iconOnly && 'btn-icon', block && 'btn-block', className]
    .filter(Boolean)
    .join(' ');
  return (
    <button type={type} className={cls} disabled={disabled || loading} aria-busy={loading || undefined} {...rest}>
      {loading ? <LoaderCircle className="spin" aria-hidden /> : Icon ? <Icon aria-hidden /> : null}
      {iconOnly ? null : children}
    </button>
  );
}

export function Spinner({ label = 'Loading' }: { label?: string }) {
  return <LoaderCircle className="spin" width={16} height={16} role="img" aria-label={label} />;
}

export function Switch({
  checked,
  onChange,
  label,
  disabled,
  id,
  describedBy,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: string;
  disabled?: boolean;
  id?: string;
  describedBy?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      id={id}
      className="switch"
      aria-checked={checked}
      aria-label={label}
      aria-describedby={describedBy}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    >
      <span className="switch-thumb" />
    </button>
  );
}

export interface SegmentOption<T extends string> {
  value: T;
  label: ReactNode;
  icon?: LucideIcon;
  disabled?: boolean;
  title?: string;
}

/** A radiogroup with roving tabindex and arrow-key navigation. */
export function Segmented<T extends string>({
  value,
  onChange,
  options,
  label,
  className,
}: {
  value: T;
  onChange: (v: T) => void;
  options: SegmentOption<T>[];
  label: string;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const onKey = (e: KeyboardEvent) => {
    const keys = ['ArrowRight', 'ArrowDown', 'ArrowLeft', 'ArrowUp', 'Home', 'End'];
    if (!keys.includes(e.key)) return;
    e.preventDefault();
    const enabled = options.filter((o) => !o.disabled);
    const idx = enabled.findIndex((o) => o.value === value);
    let next: number;
    if (e.key === 'Home') next = 0;
    else if (e.key === 'End') next = enabled.length - 1;
    else next = (idx + (e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : -1) + enabled.length) % enabled.length;
    const target = enabled[next];
    if (!target) return;
    onChange(target.value);
    requestAnimationFrame(() => ref.current?.querySelector<HTMLButtonElement>(`[data-value="${CSS.escape(target.value)}"]`)?.focus());
  };
  return (
    <div ref={ref} role="radiogroup" aria-label={label} className={`segmented ${className ?? ''}`} onKeyDown={onKey}>
      {options.map((o) => {
        const Icon = o.icon;
        const checked = o.value === value;
        return (
          <button
            key={o.value}
            type="button"
            role="radio"
            data-value={o.value}
            aria-checked={checked}
            tabIndex={checked ? 0 : -1}
            disabled={o.disabled}
            title={o.title}
            onClick={() => onChange(o.value)}
          >
            {Icon ? <Icon aria-hidden /> : null}
            {o.label}
          </button>
        );
      })}
    </div>
  );
}

export function Field({
  label,
  hint,
  error,
  optional,
  children,
  htmlFor,
  aside,
}: {
  label: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  optional?: boolean;
  htmlFor?: string;
  aside?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="field">
      <div className="field-label">
        <label htmlFor={htmlFor}>
          {label} {optional ? <span className="optional">(optional)</span> : null}
        </label>
        {aside}
      </div>
      {children}
      {error ? (
        <div className="field-error" id={htmlFor ? `${htmlFor}-error` : undefined} role="alert">
          <CircleAlert aria-hidden />
          <span>{error}</span>
        </div>
      ) : hint ? (
        <div className="field-hint" id={htmlFor ? `${htmlFor}-hint` : undefined}>
          {hint}
        </div>
      ) : null}
    </div>
  );
}

export function Badge({ tone, children, icon: Icon, title }: { tone?: 'ok' | 'err' | 'warn' | 'info' | 'brand' | 'outline'; children: ReactNode; icon?: LucideIcon; title?: string }) {
  return (
    <span className={`badge ${tone ? `badge-${tone}` : ''}`} title={title}>
      {Icon ? <Icon aria-hidden /> : null}
      {children}
    </span>
  );
}

export function Skeleton({ w = '100%', h = 14, style }: { w?: number | string; h?: number | string; style?: CSSProperties }) {
  return <span className="skeleton" aria-hidden style={{ width: w, height: h, ...style }} />;
}

export function EmptyState({ icon: Icon, title, children, actions, headingLevel = 2 }: { icon: LucideIcon; title: string; children?: ReactNode; actions?: ReactNode; headingLevel?: 2 | 3 }) {
  const H = headingLevel === 2 ? 'h2' : 'h3';
  return (
    <div className="empty">
      <div className="empty-icon">
        <Icon aria-hidden />
      </div>
      <H>{title}</H>
      {children ? <p>{children}</p> : null}
      {actions ? <div className="actions">{actions}</div> : null}
    </div>
  );
}

const CALLOUT_ICONS = { err: CircleAlert, warn: TriangleAlert, ok: CircleCheck, info: Info } as const;

export function Callout({ tone = 'info', title, children, action, icon, role, quiet }: { tone?: 'err' | 'warn' | 'ok' | 'info'; title?: ReactNode; children?: ReactNode; action?: ReactNode; icon?: LucideIcon; role?: 'alert' | 'status'; quiet?: boolean }) {
  const Icon = icon ?? CALLOUT_ICONS[tone];
  return (
    <div className={`callout callout-${tone} ${quiet ? 'callout-quiet' : ''}`} role={role}>
      <Icon aria-hidden />
      <div className="callout-body">
        {title ? <strong>{title}</strong> : null}
        {children ? <div>{children}</div> : null}
      </div>
      {action}
    </div>
  );
}

const KIND_LABEL: Record<ConnectionKind, string> = { openai: 'OpenAI', anthropic: 'Anthropic', gemini: 'Gemini', codex: 'Codex' };
const KIND_GLYPH: Record<ConnectionKind, string> = { openai: 'OA', anthropic: 'A\\', gemini: 'G', codex: '>_' };

export function kindLabel(kind: string): string {
  return KIND_LABEL[kind as ConnectionKind] ?? kind;
}

export function KindMark({ kind, size }: { kind: string; size?: 'sm' | 'lg' }) {
  const k = (kind in KIND_GLYPH ? kind : 'openai') as ConnectionKind;
  return (
    <span className={`kind-mark kind-${k} ${size ?? ''}`} aria-hidden>
      {KIND_GLYPH[k]}
    </span>
  );
}

export function StatusCode({ record }: { record: Pick<RequestRecord, 'status' | 'error'> }) {
  const o = outcome(record);
  const code = statusCode(record.status);
  const label = o === 'success' ? 'Success' : o === 'error' ? 'Error' : 'In progress';
  return (
    <span className={`status-code ${o === 'success' ? 'ok' : o === 'error' ? 'err' : 'pending'}`}>
      <span className={`dot ${o === 'success' ? 'dot-ok' : o === 'error' ? 'dot-err' : ''}`} aria-hidden />
      <span className="sr-only">{label}, status </span>
      {code ?? '—'}
    </span>
  );
}

export function useStableId(prefix: string) {
  return `${prefix}-${useId().replace(/:/g, '')}`;
}

export function PageHead({ title, description, actions }: { title: string; description?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="page-head">
      <div className="titles">
        <h1 tabIndex={-1} data-page-title>
          {title}
        </h1>
        {description ? <p>{description}</p> : null}
      </div>
      {actions ? <div className="page-actions">{actions}</div> : null}
    </header>
  );
}
