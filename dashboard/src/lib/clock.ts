// Deadlines come from the controller's clock. The Fleet response carries the
// server time, and countdowns use the difference so that a wrong clock on
// the viewer's machine does not shift them.

let offsetMs = 0;

export function setServerTime(serverNowMs: number): void {
  offsetMs = serverNowMs - Date.now();
}

/** Current time by the controller's clock, in milliseconds since the epoch. */
export function serverNow(): number {
  return Date.now() + offsetMs;
}
