import { Check, Copy } from 'lucide-react';
import { memo, useEffect, useMemo, useRef, useState } from 'react';
import { tokenize } from '../lib/highlight';
import { Button } from './ui';

/** Copy text, falling back to a hidden textarea where the async Clipboard API is unavailable (plain-http remote hosts). */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    return ok;
  } catch {
    return false;
  }
}

export function CopyButton({ text, label = 'Copy', size = 'sm', showLabel, variant = 'ghost' }: { text: string; label?: string; size?: 'sm' | 'md'; showLabel?: boolean; variant?: 'ghost' | 'default' | 'primary' }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const onClick = async () => {
    const ok = await copyText(text);
    setState(ok ? 'copied' : 'failed');
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState('idle'), 1800);
  };
  const visible = state === 'copied' ? 'Copied' : state === 'failed' ? 'Press ⌘C' : label;
  return (
    <>
      <Button
        variant={variant}
        size={size}
        icon={state === 'copied' ? Check : Copy}
        iconOnly={!showLabel}
        aria-label={showLabel ? undefined : label}
        title={showLabel ? undefined : label}
        onClick={onClick}
      >
        {visible}
      </Button>
      <span className="sr-only" role="status" aria-live="polite">
        {state === 'copied' ? 'Copied to clipboard' : state === 'failed' ? 'Copy failed. Select the text and copy manually.' : ''}
      </span>
    </>
  );
}

export const CodeBlock = memo(function CodeBlock({ code, language, title, path, compact }: { code: string; language: string; title?: string; path?: string; compact?: boolean }) {
  const tokens = useMemo(() => tokenize(code, language), [code, language]);
  return (
    <figure className={`code ${compact ? 'compact' : ''}`} style={{ margin: 0 }}>
      {title || path ? (
        <figcaption className="code-head">
          <div className="stack-sm" style={{ gap: 0, flex: 1, minWidth: 0 }}>
            {title ? <span className="title">{title}</span> : null}
            {path ? <span className="path truncate">{path}</span> : null}
          </div>
          <CopyButton text={code} label={title ? `Copy ${title.toLowerCase()}` : 'Copy'} />
        </figcaption>
      ) : null}
      <pre tabIndex={0} aria-label={title ?? 'Code'}>
        <code>
          {tokens.map((t, i) => (t.kind === 'plain' ? t.text : <span key={i} className={`tok-${t.kind}`}>{t.text}</span>))}
        </code>
      </pre>
      {!title && !path ? (
        <div style={{ position: 'absolute', top: 6, right: 6 }}>
          <CopyButton text={code} />
        </div>
      ) : null}
    </figure>
  );
});

export function CopyField({ value, label }: { value: string; label: string }) {
  return (
    <div className="copy-inline">
      <code title={value} aria-label={label}>
        {value}
      </code>
      <CopyButton text={value} label={`Copy ${label}`} />
    </div>
  );
}
