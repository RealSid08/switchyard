/**
 * A tiny, dependency-free highlighter for the snippet languages we generate.
 * It only needs to be pleasant, not complete: strings, comments, numbers,
 * keys and a few keywords. Output is a token list rendered as React spans,
 * so no HTML strings are ever injected.
 */

export type TokenKind = 'str' | 'com' | 'num' | 'key' | 'kw' | 'plain';
export interface Token {
  kind: TokenKind;
  text: string;
}

const KEYWORDS: Record<string, Set<string>> = {
  python: new Set(['import', 'from', 'as', 'with', 'for', 'in', 'if', 'print', 'def', 'return', 'True', 'False', 'None']),
  typescript: new Set(['import', 'from', 'const', 'let', 'await', 'async', 'for', 'of', 'if', 'new', 'return', 'export', 'default', 'true', 'false']),
  bash: new Set(['export', 'curl', 'codex', 'claude', 'websocat']),
  toml: new Set(['true', 'false']),
  json: new Set(['true', 'false', 'null']),
};

const commentStart: Record<string, RegExp | null> = {
  python: /#/,
  bash: /#/,
  toml: /#/,
  typescript: /\/\//,
  json: null,
  text: null,
};

export function tokenize(code: string, language: string): Token[] {
  if (language === 'text') return [{ kind: 'plain', text: code }];
  const out: Token[] = [];
  const kws = KEYWORDS[language] ?? new Set<string>();
  const com = commentStart[language] ?? null;
  const push = (kind: TokenKind, text: string) => {
    if (!text) return;
    const last = out[out.length - 1];
    if (last && last.kind === kind) last.text += text;
    else out.push({ kind, text });
  };

  let i = 0;
  let lineStart = true;
  while (i < code.length) {
    const ch = code[i];
    const rest = code.slice(i);
    // Comments (only when the comment marker starts a token, not inside a word/URL).
    if (com) {
      const m = rest.match(com);
      const prev = code[i - 1];
      if (m && m.index === 0 && (i === 0 || /\s/.test(prev ?? ''))) {
        const end = code.indexOf('\n', i);
        const stop = end === -1 ? code.length : end;
        push('com', code.slice(i, stop));
        i = stop;
        continue;
      }
    }
    if (ch === '"' || ch === "'" || ch === '`') {
      let j = i + 1;
      while (j < code.length && code[j] !== ch) {
        if (code[j] === '\\') j++;
        if (code[j] === '\n' && ch !== '`' && language !== 'bash') break;
        j++;
      }
      const text = code.slice(i, j + 1);
      // JSON object keys / TOML keys are strings followed by a colon.
      const after = code.slice(j + 1).match(/^\s*:/);
      push(language === 'json' && after ? 'key' : 'str', text);
      i = j + 1;
      lineStart = false;
      continue;
    }
    if (/[0-9]/.test(ch) && !/[A-Za-z_$-]/.test(code[i - 1] ?? '')) {
      const m = rest.match(/^\d+(\.\d+)?/);
      if (m && !/[A-Za-z_]/.test(code[i + m[0].length] ?? '')) {
        push('num', m[0]);
        i += m[0].length;
        lineStart = false;
        continue;
      }
    }
    if (/[A-Za-z_$]/.test(ch)) {
      const m = rest.match(/^[A-Za-z_$][\w$.-]*/)!;
      let word = m[0];
      // Don't swallow trailing dots/dashes into the identifier.
      word = word.replace(/[.-]+$/, '');
      const nextNonSpace = code.slice(i + word.length).match(/^\s*(=|\[)/);
      if (language === 'toml' && lineStart && nextNonSpace?.[1] === '=') push('key', word);
      else if (kws.has(word)) push('kw', word);
      else push('plain', word);
      i += word.length;
      lineStart = false;
      continue;
    }
    if (language === 'toml' && lineStart && ch === '[') {
      const end = code.indexOf('\n', i);
      const stop = end === -1 ? code.length : end;
      push('key', code.slice(i, stop));
      i = stop;
      continue;
    }
    push('plain', ch);
    if (ch === '\n') lineStart = true;
    else if (!/\s/.test(ch)) lineStart = false;
    i++;
  }
  return out;
}
