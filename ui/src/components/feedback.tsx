import { CircleAlert, CircleCheck, Info, X } from 'lucide-react';
import { createContext, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from 'react';
import { Dialog } from './Dialog';
import { Button } from './ui';

/* ---------------- Toasts ---------------- */

export interface ToastInput {
  tone?: 'ok' | 'err' | 'info';
  title: string;
  message?: ReactNode;
  action?: { label: string; onClick: () => void };
  duration?: number;
}

interface ToastItem extends ToastInput {
  id: number;
}

const ToastContext = createContext<(t: ToastInput) => void>(() => {});

export function useToast() {
  return useContext(ToastContext);
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const seq = useRef(0);
  const dismiss = useCallback((id: number) => setItems((list) => list.filter((t) => t.id !== id)), []);
  const push = useCallback(
    (t: ToastInput) => {
      const id = ++seq.current;
      setItems((list) => [...list.slice(-3), { ...t, id }]);
      const ms = t.duration ?? (t.tone === 'err' ? 9000 : 4500);
      window.setTimeout(() => dismiss(id), ms);
    },
    [dismiss],
  );
  const polite = items.filter((t) => t.tone !== 'err');
  const assertive = items.filter((t) => t.tone === 'err');
  const render = (t: ToastItem) => {
    const Icon = t.tone === 'err' ? CircleAlert : t.tone === 'ok' ? CircleCheck : Info;
    return (
      <div key={t.id} className={`toast toast-${t.tone ?? 'info'}`}>
        <Icon aria-hidden />
        <div className="toast-body">
          <div className="toast-title">{t.title}</div>
          {t.message ? <div className="toast-msg">{t.message}</div> : null}
          {t.action ? (
            <div>
              <button
                type="button"
                className="link link-button"
                onClick={() => {
                  t.action?.onClick();
                  dismiss(t.id);
                }}
              >
                {t.action.label}
              </button>
            </div>
          ) : null}
        </div>
        <Button variant="ghost" size="sm" iconOnly icon={X} aria-label="Dismiss notification" onClick={() => dismiss(t.id)} />
      </div>
    );
  };
  return (
    <ToastContext.Provider value={push}>
      {children}
      <div className="toasts">
        <div role="status" aria-live="polite" style={{ display: 'contents' }}>
          {polite.map(render)}
        </div>
        <div role="alert" aria-live="assertive" style={{ display: 'contents' }}>
          {assertive.map(render)}
        </div>
      </div>
    </ToastContext.Provider>
  );
}

/* ---------------- Confirm ---------------- */

export interface ConfirmOptions {
  title: string;
  message?: ReactNode;
  confirmLabel?: string;
  danger?: boolean;
}

type ConfirmFn = (o: ConfirmOptions) => Promise<boolean>;
const ConfirmContext = createContext<ConfirmFn>(async () => false);

export function useConfirm() {
  return useContext(ConfirmContext);
}

export function ConfirmProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<(ConfirmOptions & { resolve: (v: boolean) => void }) | null>(null);
  const confirm = useCallback<ConfirmFn>((o) => new Promise<boolean>((resolve) => setState({ ...o, resolve })), []);
  const close = (v: boolean) => {
    state?.resolve(v);
    setState(null);
  };
  const value = useMemo(() => confirm, [confirm]);
  return (
    <ConfirmContext.Provider value={value}>
      {children}
      <Dialog
        open={!!state}
        onClose={() => close(false)}
        role="alertdialog"
        width={420}
        title={state?.title ?? ''}
        footer={
          <>
            <Button onClick={() => close(false)} data-autofocus>
              Cancel
            </Button>
            <Button variant={state?.danger ? 'danger' : 'primary'} onClick={() => close(true)}>
              {state?.confirmLabel ?? 'Confirm'}
            </Button>
          </>
        }
      >
        {state?.message ? <div className="text-2">{state.message}</div> : null}
      </Dialog>
    </ConfirmContext.Provider>
  );
}
