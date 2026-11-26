import { describe, expect, it } from 'vitest';
import { Fixtures, type LogPage } from '../api/schemas';
import { rawFixtures } from '../test/fixtures';
import { MAX_LINES, initialLogState, logReducer } from './LogViewer';

const page = (texts: string[], extra: Partial<LogPage> = {}): LogPage => ({
  entries: texts.map((text) => ({ step: 'build', stream: 'stdout', text })),
  next_cursor: `after-${texts.length}`,
  truncated: false,
  complete: false,
  ...extra,
});

describe('logReducer', () => {
  it('appends pages and keeps the cursor of the newest one', () => {
    let state = logReducer(initialLogState, { type: 'page', page: page(['a\n', 'b\n']), tail: false });
    state = logReducer(state, { type: 'page', page: page(['c\n'], { next_cursor: 'c3' }), tail: false });
    expect(state.lines.map((l) => l.text)).toEqual(['a\n', 'b\n', 'c\n']);
    expect(state.cursor).toBe('c3');
    expect(state.lastHadOutput).toBe(true);
    expect(new Set(state.lines.map((l) => l.key)).size).toBe(3);
  });

  it('keeps the truncation marker once a page reports it', () => {
    let state = logReducer(initialLogState, {
      type: 'page',
      page: page(['x'], { truncated: true }),
      tail: false,
    });
    state = logReducer(state, { type: 'page', page: page([], { complete: true }), tail: false });
    expect(state.truncated).toBe(true);
    expect(state.complete).toBe(true);
    expect(state.lastHadOutput).toBe(false);
  });

  it('cleans terminal escapes out of the fixture log and keeps markup as text', () => {
    const { log_pages } = Fixtures.parse(rawFixtures());
    let state = initialLogState;
    for (const p of log_pages) state = logReducer(state, { type: 'page', page: p, tail: false });
    const text = state.lines.map((l) => l.text).join('');
    expect(text).not.toContain('\u001b');
    expect(text).not.toContain('\u0007');
    expect(text).toContain('   Compiling atlas-core v0.4.2');
    expect(text).toContain("<script>alert('from a build')</script>");
    expect(state.truncated).toBe(true);
    expect(state.complete).toBe(true);
  });

  it('drops the oldest lines beyond the cap and counts them', () => {
    const many = Array.from({ length: MAX_LINES + 25 }, (_, i) => `line ${i}\n`);
    const state = logReducer(initialLogState, { type: 'page', page: page(many), tail: false });
    expect(state.lines).toHaveLength(MAX_LINES);
    expect(state.dropped).toBe(25);
    expect(state.lines[0]?.text).toBe('line 25\n');
  });

  it('replaces the view when jumping to the end', () => {
    let state = logReducer(initialLogState, { type: 'page', page: page(['old']), tail: false });
    state = logReducer(state, { type: 'page', page: page(['newest']), tail: true });
    expect(state.lines.map((l) => l.text)).toEqual(['newest']);
    expect(state.startedAtTail).toBe(true);
  });

  it('keeps what is loaded when a request fails', () => {
    let state = logReducer(initialLogState, { type: 'page', page: page(['a']), tail: false });
    state = logReducer(state, { type: 'error', error: new Error('down') });
    expect(state.lines).toHaveLength(1);
    expect(state.error).toBeInstanceOf(Error);
    expect(state.loading).toBe(false);
  });
});
