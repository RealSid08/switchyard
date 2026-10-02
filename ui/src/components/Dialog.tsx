import { X } from 'lucide-react';
import { useEffect, useRef, type CSSProperties, type ReactNode } from 'react';
import { Button, useStableId } from './ui';

interface DialogProps {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  description?: ReactNode;
  icon?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  width?: number;
  sheet?: boolean;
  /** Block Esc/backdrop dismissal (e.g. while a request is in flight). */
  locked?: boolean;
  role?: 'dialog' | 'alertdialog';
}

/**
 * Native <dialog> + showModal(): real focus trapping, inert background, Esc to
 * close and focus restoration come from the platform. Content only mounts while
 * open, so forms reset every time.
 */
export function Dialog({ open, onClose, title, description, icon, children, footer, width, sheet, locked, role = 'dialog' }: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useStableId('dlg-title');
  const descId = useStableId('dlg-desc');
  const lockedRef = useRef(locked);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    lockedRef.current = locked;
    onCloseRef.current = onClose;
  });

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (open && !el.open) {
      const opener = document.activeElement as HTMLElement | null;
      el.showModal();
      // Prefer an explicitly marked field, else the first input, else the panel.
      const target = el.querySelector<HTMLElement>('[data-autofocus]') ?? el.querySelector<HTMLElement>('input:not([type=hidden]):not(:disabled), textarea, select');
      target?.focus();
      return () => {
        if (el.open) el.close();
        if (opener && document.contains(opener)) opener.focus();
      };
    }
    if (!open && el.open) el.close();
  }, [open]);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onCancel = (e: Event) => {
      e.preventDefault();
      if (!lockedRef.current) onCloseRef.current();
    };
    const onPointer = (e: MouseEvent) => {
      // Clicks on the ::backdrop target the dialog element itself.
      if (e.target === el && !lockedRef.current) onCloseRef.current();
    };
    el.addEventListener('cancel', onCancel);
    el.addEventListener('click', onPointer);
    return () => {
      el.removeEventListener('cancel', onCancel);
      el.removeEventListener('click', onPointer);
    };
  }, []);

  return (
    <dialog
      ref={ref}
      className={`dialog ${sheet ? 'sheet' : ''}`}
      role={role === 'alertdialog' ? 'alertdialog' : undefined}
      aria-labelledby={titleId}
      aria-describedby={description ? descId : undefined}
    >
      {open ? (
        <div className="dialog-panel" style={width ? ({ '--dialog-w': `${width}px` } as CSSProperties) : undefined}>
          <div className="dialog-head">
            {icon}
            <div className="titles">
              <h2 id={titleId}>{title}</h2>
              {description ? <p id={descId}>{description}</p> : null}
            </div>
            <Button variant="ghost" size="sm" iconOnly icon={X} aria-label="Close" onClick={onClose} disabled={locked} />
          </div>
          {children ? <div className="dialog-body">{children}</div> : null}
          {footer ? <div className="dialog-foot">{footer}</div> : null}
        </div>
      ) : null}
    </dialog>
  );
}
