// The static managed ceiling of a machine, computed the same way as
// `MachinePolicy::managed_ceiling` in `bella-core`. The administration view
// uses it to show the result of a policy edit before it is saved; the
// controller's own answer replaces it once the change is applied.

import type { Hardware, MachinePolicy } from '../api/schemas';
import { isUnlimited } from './format';

export interface Ceiling {
  cpu_milli: number;
  memory_bytes: number;
  /** `null` when the policy sets no disk cap. */
  disk_bytes: number | null;
}

/** What the policy withholds for the foreground and the OS. */
export interface ForegroundShare {
  cpu_milli: number;
  memory_bytes: number;
}

const clampPercent = (percent: number) => Math.min(Math.max(Math.trunc(percent), 0), 100);

export function foregroundShare(
  policy: Pick<
    MachinePolicy,
    'foreground_cpu_percent' | 'foreground_memory_percent' | 'foreground_memory_min_bytes'
  >,
  hardware: Pick<Hardware, 'logical_cpus' | 'memory_bytes'>,
): ForegroundShare {
  const totalCpu = hardware.logical_cpus * 1000;
  const managedCpu = Math.floor((totalCpu * (100 - clampPercent(policy.foreground_cpu_percent))) / 100);
  // Integer division first, as the controller does.
  const byPercent =
    Math.floor(hardware.memory_bytes / 100) * clampPercent(policy.foreground_memory_percent);
  const memory = Math.min(
    Math.max(byPercent, policy.foreground_memory_min_bytes),
    hardware.memory_bytes,
  );
  return { cpu_milli: totalCpu - managedCpu, memory_bytes: memory };
}

export function managedCeiling(
  policy: Pick<
    MachinePolicy,
    | 'foreground_cpu_percent'
    | 'foreground_memory_percent'
    | 'foreground_memory_min_bytes'
    | 'max_managed'
  >,
  hardware: Pick<Hardware, 'logical_cpus' | 'memory_bytes'>,
): Ceiling {
  const share = foregroundShare(policy, hardware);
  const ceiling: Ceiling = {
    cpu_milli: hardware.logical_cpus * 1000 - share.cpu_milli,
    memory_bytes: hardware.memory_bytes - share.memory_bytes,
    disk_bytes: null,
  };
  const cap = policy.max_managed;
  if (cap) {
    ceiling.cpu_milli = Math.min(ceiling.cpu_milli, cap.cpu_milli);
    ceiling.memory_bytes = Math.min(ceiling.memory_bytes, cap.memory_bytes);
    if (cap.disk_bytes > 0 && !isUnlimited(cap.disk_bytes)) ceiling.disk_bytes = cap.disk_bytes;
  }
  return ceiling;
}

/** Bytes that must stay free on a work volume of the given size. */
export function diskFloorBytes(
  policy: Pick<MachinePolicy, 'disk_free_floor_percent' | 'disk_free_floor_min_bytes'>,
  volumeTotalBytes: number,
): number {
  return Math.max(
    Math.floor(volumeTotalBytes / 100) * clampPercent(policy.disk_free_floor_percent),
    policy.disk_free_floor_min_bytes,
  );
}
