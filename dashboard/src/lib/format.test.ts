import { describe, expect, it } from 'vitest';
import {
  formatBytes,
  formatCpu,
  formatDuration,
  formatPercent,
  formatRelative,
  formatResources,
  formatTimestamp,
  isUnlimited,
  shellQuote,
  shortId,
} from './format';

const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

describe('formatBytes', () => {
  it('uses binary units with at most one decimal', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(912)).toBe('912 B');
    expect(formatBytes(184_320)).toBe('180 KiB');
    expect(formatBytes(48 * MIB + 311_296)).toBe('48.3 MiB');
    expect(formatBytes(16 * GIB)).toBe('16 GiB');
    expect(formatBytes(12.8 * GIB)).toBe('12.8 GiB');
    expect(formatBytes(1240 * GIB)).toBe('1.2 TiB');
  });

  it('does not print 1024 of a unit', () => {
    expect(formatBytes(1024 * MIB - 1)).toBe('1 GiB');
  });

  it('names the no-limit sentinel and rejects nonsense', () => {
    expect(formatBytes(2 ** 64)).toBe('no limit');
    expect(isUnlimited(2 ** 63)).toBe(true);
    expect(isUnlimited(4000 * GIB)).toBe(false);
    expect(formatBytes(-1)).toBe('unknown');
    expect(formatBytes(Number.NaN)).toBe('unknown');
  });
});

describe('formatCpu', () => {
  it('writes milli-CPUs as CPUs', () => {
    expect(formatCpu(8000)).toBe('8 CPUs');
    expect(formatCpu(1000)).toBe('1 CPU');
    expect(formatCpu(13_900)).toBe('13.9 CPUs');
    expect(formatCpu(500)).toBe('0.5 CPUs');
    expect(formatCpu(0)).toBe('0 CPUs');
  });
});

describe('formatDuration', () => {
  it('shows the two largest units', () => {
    expect(formatDuration(0)).toBe('0s');
    expect(formatDuration(400)).toBe('under 1s');
    expect(formatDuration(45_000)).toBe('45s');
    expect(formatDuration(192_000)).toBe('3m 12s');
    expect(formatDuration(180_000)).toBe('3m');
    expect(formatDuration(2 * 3_600_000 + 5 * 60_000 + 9_000)).toBe('2h 5m');
    expect(formatDuration(3 * 86_400_000 + 4 * 3_600_000)).toBe('3d 4h');
    expect(formatDuration(86_400_000)).toBe('1d');
  });

  it('never goes negative', () => {
    expect(formatDuration(-5000)).toBe('0s');
  });
});

describe('formatRelative', () => {
  it('speaks of the past and of the future', () => {
    expect(formatRelative(1_000_000, 1_192_000)).toBe('3m 12s ago');
    expect(formatRelative(1_192_000, 1_000_000)).toBe('in 3m 12s');
    expect(formatRelative(1_000_000, 1_000_400)).toBe('just now');
  });
});

describe('other formatters', () => {
  it('formats a UTC timestamp', () => {
    expect(formatTimestamp(1_795_000_000_000, true)).toBe('2026-11-18 11:06:40Z');
    expect(formatTimestamp(Number.NaN)).toBe('unknown');
  });

  it('formats a share', () => {
    expect(formatPercent(0.414)).toBe('41%');
    expect(formatPercent(0.004)).toBe('<1%');
    expect(formatPercent(0)).toBe('0%');
    expect(formatPercent(1.2)).toBe('120%');
  });

  it('shortens identifiers for display', () => {
    // The end, because time-ordered ids made close together share a start.
    expect(shortId('job_0123456789abcdef0123456789abcdef')).toBe('job_…abcdef');
    expect(shortId('job_01a218c9aaaa00000000000000111111')).not.toBe(
      shortId('job_01a218c9aaaa00000000000000222222'),
    );
    expect(shortId('boot_0123456789abcdef0123456789abcdef')).toBe('boot_…abcdef');
    expect(shortId('job_1234')).toBe('job_1234');
    expect(shortId('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad')).toBe('ba7816bf8f01');
  });

  it('describes a resource vector', () => {
    expect(
      formatResources({
        cpu_milli: 4000,
        memory_bytes: 8 * GIB,
        disk_bytes: 20 * GIB,
        tokens: { 'ios-simulator': 1 },
      }),
    ).toBe('4 CPUs, 8 GiB memory, 20 GiB disk, 1 ios-simulator');
    expect(formatResources({ cpu_milli: 0, memory_bytes: 0, disk_bytes: 0 })).toBe('nothing');
    // An unlimited disk allowance is not a request for disk.
    expect(formatResources({ cpu_milli: 1000, memory_bytes: 0, disk_bytes: 2 ** 64 })).toBe('1 CPU');
  });

  it('quotes a string for a POSIX shell', () => {
    expect(shellQuote('bella1:https://c.example#code=AB')).toBe("'bella1:https://c.example#code=AB'");
    expect(shellQuote("it's; rm -rf ~")).toBe("'it'\\''s; rm -rf ~'");
  });
});
