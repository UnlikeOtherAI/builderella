import { describe, expect, it } from 'vitest';
import { sanitizeLogText } from './ansi';

describe('sanitizeLogText', () => {
  it('removes colour and cursor sequences', () => {
    expect(sanitizeLogText('\u001b[1m\u001b[32m   Compiling\u001b[0m atlas v0.4.2\n')).toBe(
      '   Compiling atlas v0.4.2\n',
    );
    expect(sanitizeLogText('a\u001b[2K\u001b[1Gb')).toBe('ab');
    expect(sanitizeLogText('\u001b[38;2;255;0;0mred\u001b[m')).toBe('red');
  });

  it('removes window-title and hyperlink sequences', () => {
    expect(sanitizeLogText('\u001b]0;cargo test\u0007done')).toBe('done');
    expect(sanitizeLogText('\u001b]8;;https://evil.example\u001b\\click\u001b]8;;\u001b\\')).toBe('click');
  });

  it('keeps each carriage-return rewrite as its own line', () => {
    expect(sanitizeLogText('progress 1/2\rprogress 2/2\r\ndone')).toBe('progress 1/2\nprogress 2/2\ndone');
  });

  it('drops other control characters but keeps tabs and newlines', () => {
    expect(sanitizeLogText('a\u0000b\u0008c\td\ne\u007f\u009b')).toBe('abc\td\ne');
    expect(sanitizeLogText('lone escape \u001b')).toBe('lone escape ');
  });

  it('leaves markup as plain characters', () => {
    const text = '<script>alert(1)</script> <a href="https://x.example">x</a>';
    expect(sanitizeLogText(text)).toBe(text);
  });
});
