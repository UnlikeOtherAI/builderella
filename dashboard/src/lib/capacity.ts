// Arithmetic behind the stacked capacity bars on the Fleet view.
//
// One bar describes one resource of one physical machine. It separates four
// things that are easy to confuse:
//
//   managed_used     what managed work is measured to be using
//   reserved_unused  capacity promised to managed work but not in use yet
//   external         everything else on the host: the owner's applications
//   protected        the part of the foreground share nothing is using
//
// The protected share is withheld for the owner, and the owner's activity is
// what consumes it, so only its unused remainder is drawn as its own
// segment. Drawing the whole share next to external activity would count
// the same capacity twice.

import type { MachineView } from '../api/schemas';
import { foregroundShare } from './policy';

export type SegmentKind = 'managed_used' | 'reserved_unused' | 'external' | 'protected';

export interface CapacityInput {
  /** Hardware total of the resource. */
  total: number;
  /** Measured managed use; `null` without telemetry. */
  managedUsed: number | null;
  /** Sum of held reservations. */
  reserved: number;
  /** Measured use outside managed work; `null` without telemetry. */
  external: number | null;
  /** What policy withholds for the foreground. */
  protectedShare: number;
}

export interface Segment {
  kind: SegmentKind;
  value: number;
  /** Share of the drawn bar, 0..1. */
  fraction: number;
}

export interface Capacity {
  total: number;
  /** Width the fractions are relative to: the total, or the committed sum when that is larger. */
  scale: number;
  segments: Segment[];
  free: number;
  freeFraction: number;
  /** How far the four segments together exceed the machine. Zero when they fit. */
  overBy: number;
  /** Where the hardware total falls on the bar when over-committed, 0..1. */
  totalMarker: number | null;
  /** Whether usage figures come from telemetry. */
  measured: boolean;
  /** Managed work measured above what is reserved for it. */
  managedOverReservation: number;
  protectedShare: number;
  /** The part of the protected share that external activity occupies. */
  protectedInUse: number;
}

const clean = (value: number | null) =>
  value !== null && Number.isFinite(value) && value > 0 ? value : 0;

/**
 * Lay out one capacity bar. The segment fractions plus the free fraction
 * always sum to 1 (or to 0 for an empty machine), so nothing can draw past
 * the end of the bar. When the commitments exceed the hardware total the bar
 * is rescaled to their sum and `overBy` / `totalMarker` say by how much;
 * the excess is shown, never clipped away.
 */
export function computeCapacity(input: CapacityInput): Capacity {
  const total = clean(input.total);
  const managedUsed = clean(input.managedUsed);
  const reserved = clean(input.reserved);
  const external = clean(input.external);
  const protectedShare = clean(input.protectedShare);

  const reservedUnused = Math.max(reserved - managedUsed, 0);
  const protectedInUse = Math.min(external, protectedShare);
  const protectedUnused = protectedShare - protectedInUse;

  const values: [SegmentKind, number][] = [
    ['managed_used', managedUsed],
    ['reserved_unused', reservedUnused],
    ['external', external],
    ['protected', protectedUnused],
  ];
  const committed = values.reduce((sum, [, value]) => sum + value, 0);
  const scale = Math.max(total, committed);
  const fraction = (value: number) => (scale > 0 ? value / scale : 0);
  const free = Math.max(total - committed, 0);
  const overBy = Math.max(committed - total, 0);

  return {
    total,
    scale,
    segments: values.map(([kind, value]) => ({ kind, value, fraction: fraction(value) })),
    free,
    freeFraction: fraction(free),
    overBy,
    totalMarker: overBy > 0 ? fraction(total) : null,
    measured: input.managedUsed !== null && input.external !== null,
    managedOverReservation: Math.max(managedUsed - reserved, 0),
    protectedShare,
    protectedInUse,
  };
}

export interface MachineCapacity {
  cpu: Capacity;
  memory: Capacity;
}

/** The CPU and memory bars of one machine, from its view as the API returns it. */
export function machineCapacity(machine: MachineView): MachineCapacity {
  const { hardware } = machine.inventory;
  const telemetry = machine.telemetry ?? null;
  const share = foregroundShare(machine.policy, hardware);
  const memoryTotal =
    telemetry && telemetry.memory_total_bytes > 0 ? telemetry.memory_total_bytes : hardware.memory_bytes;

  return {
    cpu: computeCapacity({
      total: hardware.logical_cpus * 1000,
      managedUsed: telemetry ? telemetry.cpu_managed_milli : null,
      reserved: machine.reserved.cpu_milli,
      external: telemetry
        ? Math.max(telemetry.cpu_used_milli - telemetry.cpu_managed_milli, 0)
        : null,
      protectedShare: share.cpu_milli,
    }),
    memory: computeCapacity({
      total: memoryTotal,
      managedUsed: telemetry ? telemetry.memory_managed_bytes : null,
      reserved: machine.reserved.memory_bytes,
      external: telemetry
        ? Math.max(
            telemetry.memory_total_bytes -
              telemetry.memory_available_bytes -
              telemetry.memory_managed_bytes,
            0,
          )
        : null,
      protectedShare: share.memory_bytes,
    }),
  };
}

/**
 * The same bar in the three words of a summary: in use (managed work plus
 * everything else on the host), reserved but not yet used, and available.
 * The unused part of the share protected for the owner is its own segment,
 * drawn at the end.
 */
export interface LoadSummary {
  total: number;
  /** `null` without telemetry. */
  inUse: number | null;
  reserved: number;
  available: number;
  protectedUnused: number;
  /** Shares of the drawn bar, 0..1, summing to 1 (or 0 for an empty machine). */
  fractions: { inUse: number; reserved: number; available: number; protected: number };
  /** In use plus reserved, as a share of the machine; can pass 1 when over-committed. */
  load: number;
  overBy: number;
}

export function summarizeLoad(capacity: Capacity): LoadSummary {
  const part = (kind: SegmentKind) => capacity.segments.find((s) => s.kind === kind) ?? { value: 0, fraction: 0 };
  const managed = part('managed_used');
  const external = part('external');
  const reserved = part('reserved_unused');
  const protectedUnused = part('protected');
  const used = managed.value + external.value;
  return {
    total: capacity.total,
    inUse: capacity.measured ? used : null,
    reserved: reserved.value,
    available: capacity.free,
    protectedUnused: protectedUnused.value,
    fractions: {
      inUse: managed.fraction + external.fraction,
      reserved: reserved.fraction,
      available: capacity.freeFraction,
      protected: protectedUnused.fraction,
    },
    load: capacity.total > 0 ? (used + reserved.value) / capacity.total : 0,
    overBy: capacity.overBy,
  };
}
