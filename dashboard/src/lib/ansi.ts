// Build output is untrusted text. Before it is shown, terminal escape
// sequences and other control characters are removed, so a log cannot move
// the cursor, hide text or smuggle markup. The result is rendered as a text
// node only; nothing in it becomes HTML or a link.

// OSC: ESC ] ... terminated by BEL or ESC \
const OSC = /\u001b\][\s\S]*?(?:\u0007|\u001b\\)/g;
// CSI: ESC [ parameters intermediates final
const CSI = /\u001b\[[0-9;:<=>?]*[ -/]*[@-~]/g;
// Any other escape: ESC and the character after it, but never a line break.
const OTHER_ESCAPE = /\u001b[^\n\r]?/g;
// C0 controls except tab and newline, plus DEL and the C1 range.
const CONTROL = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g;

/**
 * Plain text of one log chunk. A carriage return that is not part of a line
 * ending restarts the line on a terminal; here each rewrite becomes its own
 * line, so no output is lost.
 */
export function sanitizeLogText(text: string): string {
  return text
    .replace(OSC, '')
    .replace(CSI, '')
    .replace(OTHER_ESCAPE, '')
    .replace(/\r\n/g, '\n')
    .replace(/\r/g, '\n')
    .replace(CONTROL, '');
}
