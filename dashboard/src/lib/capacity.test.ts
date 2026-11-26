import { describe, expect, it } from 'vitest';
import { Fixtures } from '../api/schemas';
import { rawFixtures } from '../test/fixtures';
import { computeCapacity, machineCapacity, type Capacity, type CapacityInput } from './capacity';
import { foregroundShare, managedCeiling } from './policy';

const GIB = 1024 ** 3;

function drawn(c: Capacity): number {
  return c.segments.reduce((sum, s) => sum + s.fraction, 0) + c.freeFraction;
}

function value(c: Capacity, kind: string): number {
  return c.segments.find((s) => s.kind === kind)?.value ?? Number.NaN;
}

describe('computeCapacity', () => {
  it('splits a machine into the four segments plus free', () => {
    const c = computeCapacity({
      total: 32_000,
      managedUsed: 13_900,
      reserved: 20_000,
      external: 1_500,
      protectedShare: 3_200,
    });
    expect(value(c, 'managed_used')).toBe(13_900);
    expect(value(c, 'reserved_unused')).toBe(6_100);
    expect(value(c, 'external')).toBe(1_500);
    // External activity uses the protected share first; only the rest is drawn.
    expect(value(c, 'protected')).toBe(1_700);
    expect(c.protectedInUse).toBe(1_500);
    expect(c.free).toBe(32_000 - 13_900 - 6_100 - 1_500 - 1_700);
    expect(c.overBy).toBe(0);
    expect(c.totalMarker).toBeNull();
    expect(drawn(c)).toBeCloseTo(1, 10);
  });

  it('never draws past the end of the bar, whatever the inputs', () => {
    const steps = [0, 1, 500, 4_000, 16_000, 40_000];
    for (const total of [0, 8_000, 32_000]) {
      for (const managedUsed of steps) {
        for (const reserved of steps) {
          for (const external of steps) {
            for (const protectedShare of [0, 1_600, 50_000]) {
              const c = computeCapacity({ total, managedUsed, reserved, external, protectedShare });
              const sum = drawn(c);
              expect(sum).toBeLessThanOrEqual(1 + 1e-9);
              if (c.scale > 0) expect(sum).toBeCloseTo(1, 9);
              else expect(sum).toBe(0);
              for (const s of c.segments) {
                expect(s.fraction).toBeGreaterThanOrEqual(0);
                expect(s.fraction).toBeLessThanOrEqual(1);
                expect(Number.isFinite(s.fraction)).toBe(true);
              }
              expect(c.free).toBeGreaterThanOrEqual(0);
              expect(c.freeFraction).toBeGreaterThanOrEqual(0);
            }
          }
        }
      }
    }
  });

  it('shows over-commitment instead of clipping it', () => {
    const input: CapacityInput = {
      total: 16 * GIB,
      managedUsed: 6 * GIB,
      reserved: 10 * GIB,
      external: 9 * GIB,
      protectedShare: 2 * GIB,
    };
    const c = computeCapacity(input);
    // 6 used + 4 reserved-unused + 9 external + 0 protected left = 19 of 16.
    expect(c.overBy).toBe(3 * GIB);
    expect(c.scale).toBe(19 * GIB);
    expect(c.free).toBe(0);
    // Every segment keeps its true value; the bar is rescaled, not cut.
    expect(value(c, 'managed_used')).toBe(6 * GIB);
    expect(value(c, 'reserved_unused')).toBe(4 * GIB);
    expect(value(c, 'external')).toBe(9 * GIB);
    expect(c.totalMarker).toBeCloseTo(16 / 19, 10);
    expect(drawn(c)).toBeCloseTo(1, 10);
  });

  it('reports managed work that uses more than its reservation', () => {
    const c = computeCapacity({
      total: 8_000,
      managedUsed: 5_000,
      reserved: 4_000,
      external: 0,
      protectedShare: 800,
    });
    expect(c.managedOverReservation).toBe(1_000);
    expect(value(c, 'reserved_unused')).toBe(0);
    expect(value(c, 'managed_used')).toBe(5_000);
  });

  it('draws reservations only when there is no telemetry', () => {
    const c = computeCapacity({
      total: 8_000,
      managedUsed: null,
      reserved: 4_000,
      external: null,
      protectedShare: 1_600,
    });
    expect(c.measured).toBe(false);
    expect(value(c, 'managed_used')).toBe(0);
    expect(value(c, 'reserved_unused')).toBe(4_000);
    expect(value(c, 'protected')).toBe(1_600);
    expect(c.free).toBe(2_400);
  });

  it('treats negative and non-finite inputs as zero', () => {
    const c = computeCapacity({
      total: 8_000,
      managedUsed: -5,
      reserved: Number.NaN,
      external: Number.POSITIVE_INFINITY,
      protectedShare: -1,
    });
    expect(c.segments.every((s) => s.value === 0)).toBe(true);
    expect(c.free).toBe(8_000);
  });
});

describe('machineCapacity on the fixture fleet', () => {
  const { fleet } = Fixtures.parse(rawFixtures());
  const byName = (name: string) => {
    const machine = fleet.machines.find((m) => m.name === name);
    if (!machine) throw new Error(`fixture machine ${name} is missing`);
    return machine;
  };

  it('never exceeds the bar on any machine', () => {
    for (const machine of fleet.machines) {
      const { cpu, memory } = machineCapacity(machine);
      for (const c of [cpu, memory]) expect(drawn(c)).toBeLessThanOrEqual(1 + 1e-9);
    }
  });

  it('shows the workstation owner using the whole protected share', () => {
    const { cpu, memory } = machineCapacity(byName('studio-1'));
    expect(cpu.total).toBe(10_000);
    expect(value(cpu, 'managed_used')).toBe(2_400);
    expect(value(cpu, 'reserved_unused')).toBe(1_600);
    expect(value(cpu, 'external')).toBe(3_900);
    expect(cpu.protectedShare).toBe(2_000);
    expect(value(cpu, 'protected')).toBe(0);
    expect(memory.protectedShare).toBeGreaterThan(6 * GIB);
    expect(value(memory, 'protected')).toBe(0);
  });

  it('agrees with the controller about every managed ceiling', () => {
    for (const machine of fleet.machines) {
      const ours = managedCeiling(machine.policy, machine.inventory.hardware);
      expect([machine.name, ours.cpu_milli]).toEqual([machine.name, machine.managed_ceiling.cpu_milli]);
      expect([machine.name, ours.memory_bytes]).toEqual([machine.name, machine.managed_ceiling.memory_bytes]);
      const share = foregroundShare(machine.policy, machine.inventory.hardware);
      expect(share.cpu_milli + ours.cpu_milli).toBe(machine.inventory.hardware.logical_cpus * 1000);
    }
  });

  it('uses the absolute memory minimum on a small machine', () => {
    const ceiling = managedCeiling(
      { foreground_cpu_percent: 20, foreground_memory_percent: 20, foreground_memory_min_bytes: 4 * GIB },
      { logical_cpus: 4, memory_bytes: 8 * GIB },
    );
    expect(ceiling.memory_bytes).toBe(4 * GIB);
    expect(ceiling.cpu_milli).toBe(3_200);
  });
});
