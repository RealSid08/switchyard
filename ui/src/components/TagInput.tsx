import { Plus, X } from 'lucide-react';
import { useRef, useState, type ClipboardEvent, type KeyboardEvent } from 'react';

/** Split pasted/typed text into model ids on commas, whitespace or newlines. */
export function splitTags(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

export function addTags(existing: string[], incoming: string[]): string[] {
  const seen = new Set(existing);
  const out = [...existing];
  for (const t of incoming) {
    if (!seen.has(t)) {
      seen.add(t);
      out.push(t);
    }
  }
  return out;
}

export function TagInput({
  id,
  value,
  onChange,
  placeholder,
  suggestions = [],
  invalid,
  describedBy,
  label,
}: {
  id: string;
  value: string[];
  onChange: (v: string[]) => void;
  placeholder?: string;
  suggestions?: string[];
  invalid?: boolean;
  describedBy?: string;
  label: string;
}) {
  const [draft, setDraft] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  const commit = (text = draft) => {
    const tags = splitTags(text);
    if (tags.length) onChange(addTags(value, tags));
    setDraft('');
  };
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter' || e.key === ',' || e.key === 'Tab') {
      if (!draft.trim()) return;
      e.preventDefault();
      commit();
    } else if (e.key === 'Backspace' && !draft && value.length) {
      onChange(value.slice(0, -1));
    }
  };
  const onPaste = (e: ClipboardEvent<HTMLInputElement>) => {
    const text = e.clipboardData.getData('text');
    if (/[\s,]/.test(text.trim())) {
      e.preventDefault();
      commit(draft + text);
    }
  };
  const remaining = suggestions.filter((s) => !value.includes(s));
  return (
    <div className="stack-sm">
      <div className="tag-input" aria-invalid={invalid || undefined} onClick={() => inputRef.current?.focus()}>
        {value.map((tag) => (
          <span className="tag" key={tag}>
            <span title={tag}>{tag}</span>
            <button type="button" aria-label={`Remove ${tag}`} onClick={() => onChange(value.filter((t) => t !== tag))}>
              <X aria-hidden />
            </button>
          </span>
        ))}
        <input
          ref={inputRef}
          id={id}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKey}
          onPaste={onPaste}
          onBlur={() => commit()}
          placeholder={value.length ? 'Add another…' : placeholder}
          aria-describedby={describedBy}
          aria-label={label}
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
        />
      </div>
      {remaining.length ? (
        <div className="chips" aria-label="Suggested models">
          {remaining.map((s) => (
            <button type="button" key={s} className="chip-button" onClick={() => onChange(addTags(value, [s]))} aria-label={`Add ${s}`}>
              <Plus aria-hidden />
              {s}
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}
