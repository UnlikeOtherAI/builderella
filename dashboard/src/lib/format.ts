// Humane formatting of sizes, CPU amounts, durations and identifiers.

import type { ResourceVector } from '../api/schemas';

const BYTE_UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'] as const;

/**
 * The API uses `u64::MAX` for "no limit". JavaScript cannot represent it
 * exactly, so anything at or above 2^63 is treated as unlimited.
 */
export function isUnlimited(value: number): boolean {
  return value >= 2 ** 63;
}

function trimDecimal(value: number, digits = 1): string {
  return value.toFixed(digits).replace(/\.0+$/, '');
}

/** `12.8 GiB`, `912 B`. Binary units, at most one decimal. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return 'unknown';
  if (isUnlimited(bytes)) return 'no limit';
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // Rounding can reach the next unit (1023.97 MiB shows as 1 GiB).
  if (unit > 0 && Number(value.toFixed(1)) >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 ? String(Math.round(value)) : trimDecimal(value)} ${BYTE_UNITS[unit]}`;
}

/** Milli-CPUs as logical CPUs: `8 CPUs`, `1 CPU`, `13.9 CPUs`. */
export function formatCpu(milli: number): string {
  if (!Number.isFinite(milli) || milli < 0) return 'unknown';
  if (isUnlimited(milli)) return 'no limit';
  const text = trimDecimal(milli / 1000);
  return `${text} ${text === '1' ? 'CPU' : 'CPUs'}`;
}

/** `3m 12s`, `2h 5m`, `45s`. The two largest units; never negative. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms)) return 'unknown';
  const total = Math.max(0, Math.floor(ms / 1000));
  if (total === 0) return ms >= 1 ? 'under 1s' : '0s';
  const days = Math.floor(total / 86_400);
  const hours = Math.floor((total % 86_400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  if (days > 0) return hours > 0 ? `${days}d ${hours}h` : `${days}d`;
  if (hours > 0) return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
  if (minutes > 0) return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  return `${seconds}s`;
}

/** `3m 12s ago`, or `in 3m 12s` for a time in the future. */
export function formatRelative(at: number, now: number): string {
  const delta = now - at;
  if (Math.abs(delta) < 1000) return 'just now';
  return delta > 0 ? `${formatDuration(delta)} ago` : `in ${formatDuration(-delta)}`;
}

function pad(value: number): string {
  return String(value).padStart(2, '0');
}

/** `2026-11-26 14:03:22` in local time, or UTC with a `Z` suffix. */
export function formatTimestamp(ms: number, utc = false): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return 'unknown';
  if (utc) {
    return (
      `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ` +
      `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}Z`
    );
  }
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  );
}

/** `41%`; `<1%` for a small non-zero share. */
export function formatPercent(fraction: number): string {
  if (!Number.isFinite(fraction) || fraction <= 0) return '0%';
  const percent = fraction * 100;
  if (percent < 1) return '<1%';
  return `${Math.round(percent)}%`;
}

/**
 * Short display form of an identifier: `job_3f9a1c2b` for a prefixed id,
 * the first 12 characters for a digest. Never use the result as an id.
 */
export function shortId(id: string): string {
  const underscore = id.indexOf('_');
  if (underscore > 0 && underscore <= 5) return id.slice(0, underscore + 9);
  return id.slice(0, 12);
}

/** `8 CPUs, 16 GiB memory, 20 GiB disk, 1 ios-simulator`. */
export function formatResources(v: ResourceVector): string {
  const parts: string[] = [];
  if (v.cpu_milli > 0) parts.push(formatCpu(v.cpu_milli));
  if (v.memory_bytes > 0) parts.push(`${formatBytes(v.memory_bytes)} memory`);
  if (v.disk_bytes > 0 && !isUnlimited(v.disk_bytes)) parts.push(`${formatBytes(v.disk_bytes)} disk`);
  for (const [name, count] of Object.entries(v.tokens ?? {})) {
    if (count > 0) parts.push(`${count} ${name}`);
  }
  return parts.length > 0 ? parts.join(', ') : 'nothing';
}

/** Quote a string for a POSIX shell, so a pasted command is one argument. */
export function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}
