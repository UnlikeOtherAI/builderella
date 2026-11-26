//! The budget and the admission decision.

use crate::format::{cpus, cpus_bare, gib, gib_bare, secs};
use crate::{Budget, Grant, Input, ProtectConfig, Rejection};
use bella_core::job::QueueReasonCode;
use bella_core::machine::{AvailabilityFlag, PressureLevel, Telemetry, ThermalState};
use bella_core::resources::ResourceVector;

/// Sums over every grant the journal holds.
pub(crate) struct Totals {
    /// `Σ r_i`.
    pub reserved: ResourceVector,
    /// `Σ max(r_i, u_i)` for memory.
    pub memory_charged: u64,
    /// `Σ max(r_i − u_i, 0)` for memory.
    pub memory_promised: u64,
    /// Scratch disk promised and not yet written.
    pub disk_promised: u64,
}

pub(crate) fn totals(grants: &[Grant]) -> Totals {
    let mut sums = Totals {
        reserved: ResourceVector::ZERO,
        memory_charged: 0,
        memory_promised: 0,
        disk_promised: 0,
    };
    for grant in grants {
        sums.reserved = sums.reserved.saturating_add(&grant.reserved);
        sums.memory_charged = sums.memory_charged.saturating_add(grant.memory_charged());
        sums.memory_promised = sums.memory_promised.saturating_add(grant.memory_promise());
        sums.disk_promised = sums.disk_promised.saturating_add(grant.disk_promise());
    }
    sums
}

/// A grant using more memory than it reserved, beyond the tolerance.
pub(crate) fn is_offender(grant: &Grant, config: &ProtectConfig) -> bool {
    grant.memory_excess() > config.memory_excess_tolerance_bytes
}

/// The sample to decide on, or why there is none. A sample is fresh when it
/// is at most `telemetry_max_age` old; one stamped further than that in the
/// future (the wall clock stepped back) is not trusted either.
pub(crate) fn fresh_telemetry<'a>(input: &Input<'a>) -> Result<&'a Telemetry, String> {
    let Some(sample) = input.telemetry else {
        return Err("no fresh local telemetry (no sample has been taken)".to_string());
    };
    let limit = input.config.telemetry_max_age;
    if sample.sampled_at > input.now {
        let ahead = sample.sampled_at.since(input.now);
        if ahead > limit {
            return Err(format!(
                "no fresh local telemetry (the last sample is stamped {} in the future, limit {})",
                secs(ahead),
                secs(limit)
            ));
        }
        return Ok(sample);
    }
    let age = input.now.since(sample.sampled_at);
    if age > limit {
        return Err(format!(
            "no fresh local telemetry (the last sample is {} old, limit {})",
            secs(age),
            secs(limit)
        ));
    }
    Ok(sample)
}

/// The CPU side of the budget, in milli-CPUs.
pub(crate) struct CpuView {
    /// Foreground entitlement `F = T × foreground_cpu_percent`.
    pub entitlement: u64,
    /// Non-managed use `E`.
    pub external: u64,
    pub static_ceiling: u64,
    /// `min(static ceiling, T − max(E, F))`, or the static ceiling while a
    /// boost is active.
    pub ceiling: u64,
    pub boosted: bool,
}

/// The managed CPU ceiling for a given external use.
///
/// The foreground entitlement includes current foreground use: foreground
/// work beyond the entitlement shrinks the managed ceiling further, and an
/// idle foreground never enlarges it, so the reserve is never borrowed. A
/// boost lets managed work keep the static ceiling while the foreground is
/// over its entitlement. It cannot go beyond the static ceiling, which
/// already excludes the entitlement.
pub(crate) fn cpu_view(input: &Input, static_ceiling: u64, external: u64) -> CpuView {
    let total = u64::from(input.hardware.logical_cpus) * 1000;
    let entitlement = total * u64::from(input.policy.foreground_cpu_percent.min(100)) / 100;
    let boosted = input
        .control
        .boost_until
        .is_some_and(|until| until > input.now);
    let dynamic = total.saturating_sub(external.max(entitlement));
    let ceiling = if boosted {
        static_ceiling
    } else {
        static_ceiling.min(dynamic)
    };
    CpuView {
        entitlement,
        external,
        static_ceiling,
        ceiling,
        boosted,
    }
}

/// Memory a new request could take without endangering the host:
/// `A − F − G − Σ max(r_i − u_i, 0)`, floored at zero.
fn memory_grantable(input: &Input, sample: &Telemetry, sums: &Totals) -> u64 {
    sample
        .memory_available_bytes
        .saturating_sub(input.policy.available_memory_floor_bytes)
        .saturating_sub(input.config.memory_guard_bytes)
        .saturating_sub(sums.memory_promised)
}

/// Scratch disk a new request could take: free space minus the floor minus
/// what is promised and not yet written, floored at zero.
fn disk_grantable(input: &Input, sample: &Telemetry, sums: &Totals) -> u64 {
    sample
        .disk_free_bytes
        .saturating_sub(input.policy.disk_floor_bytes(sample.disk_total_bytes))
        .saturating_sub(sums.disk_promised)
}

/// A condition that closes admission for every request.
struct Gate {
    flag: Option<AvailabilityFlag>,
    code: QueueReasonCode,
    message: String,
}

fn gate(flag: Option<AvailabilityFlag>, code: QueueReasonCode, message: String) -> Gate {
    Gate {
        flag,
        code,
        message,
    }
}

/// Every reason no offer would be accepted right now, most fundamental
/// first. `admit` reports the first one.
fn gates(input: &Input, telemetry: &Result<&Telemetry, String>) -> Vec<Gate> {
    let policy = input.policy;
    let config = input.config;
    let mut out = Vec::new();

    if !policy.accept_builds {
        out.push(gate(
            Some(AvailabilityFlag::BuildsDisabled),
            QueueReasonCode::BuildsDisabled,
            "this machine's policy does not accept builds".to_string(),
        ));
    }
    if input.control.paused {
        out.push(gate(
            Some(AvailabilityFlag::UserPaused),
            QueueReasonCode::MachinePaused,
            "paused by the machine's owner; running work continues".to_string(),
        ));
    }
    if input.control.draining {
        out.push(gate(
            Some(AvailabilityFlag::Draining),
            QueueReasonCode::MachineDraining,
            format!(
                "draining by the machine's owner; {} attempt(s) still hold resources",
                input.grants.len()
            ),
        ));
    }

    let sample = match telemetry {
        Ok(sample) => *sample,
        Err(why) => {
            // Nothing below can be judged without an observation.
            out.push(gate(None, QueueReasonCode::StaleTelemetry, why.clone()));
            return out;
        }
    };

    // An unknown power source counts as "not on external power" only when a
    // battery percentage is reported: a machine that reports neither has no
    // battery to run down.
    if policy.require_ac_power {
        match (sample.power.on_ac, sample.power.battery_percent) {
            (Some(false), Some(percent)) => out.push(gate(
                Some(AvailabilityFlag::OnBattery),
                QueueReasonCode::OnBattery,
                format!(
                    "on battery power (battery at {percent}%); policy accepts new work only on external power"
                ),
            )),
            (Some(false), None) => out.push(gate(
                Some(AvailabilityFlag::OnBattery),
                QueueReasonCode::OnBattery,
                "on battery power; policy accepts new work only on external power".to_string(),
            )),
            (None, Some(percent)) => out.push(gate(
                Some(AvailabilityFlag::OnBattery),
                QueueReasonCode::OnBattery,
                format!(
                    "power source unknown with a battery at {percent}%; treated as not on external power, which policy requires"
                ),
            )),
            (Some(true), _) | (None, None) => {}
        }
    }
    // The battery minimum applies whenever a level is reported, on external
    // power too: a low battery that is charging is still a low battery.
    if let (Some(minimum), Some(percent)) =
        (policy.min_battery_percent, sample.power.battery_percent)
        && percent < minimum
    {
        out.push(gate(
            Some(AvailabilityFlag::OnBattery),
            QueueReasonCode::OnBattery,
            format!("battery at {percent}%, below the policy minimum of {minimum}%"),
        ));
    }

    // There is no queue reason code for heat; refusing work on a hot machine
    // is reported as foreground protection.
    let hot = match sample.thermal {
        ThermalState::Serious => Some("serious"),
        ThermalState::Critical => Some("critical"),
        ThermalState::Unknown | ThermalState::Nominal | ThermalState::Fair => None,
    };
    if let Some(level) = hot {
        out.push(gate(
            Some(AvailabilityFlag::Overheated),
            QueueReasonCode::ForegroundProtection,
            format!("thermal state is {level}; no new work until the machine cools down"),
        ));
    }

    // `Unknown` pressure does not close admission: hosts that report no
    // pressure level are judged on available memory alone.
    let pressure = match sample.memory_pressure {
        PressureLevel::Warning => Some("warning"),
        PressureLevel::Critical => Some("critical"),
        PressureLevel::Unknown | PressureLevel::Normal => None,
    };
    if let Some(level) = pressure {
        out.push(gate(
            Some(AvailabilityFlag::PressureLimited),
            QueueReasonCode::MemoryPressure,
            format!(
                "memory pressure is {level} (available {} of {})",
                gib(sample.memory_available_bytes),
                gib(sample.memory_total_bytes)
            ),
        ));
    }
    let floor = policy.available_memory_floor_bytes;
    if sample.memory_available_bytes < floor {
        out.push(gate(
            Some(AvailabilityFlag::PressureLimited),
            QueueReasonCode::MemoryPressure,
            format!(
                "available memory {} is below the {} floor",
                gib(sample.memory_available_bytes),
                gib(floor)
            ),
        ));
    }

    for grant in input.grants.iter().filter(|g| is_offender(g, config)) {
        out.push(gate(
            None,
            QueueReasonCode::MemoryPressure,
            format!(
                "attempt {} uses {} of memory against a {} reservation ({} over); no new work until it is back within its reservation or gone",
                grant.attempt_id,
                gib(grant.memory_in_use()),
                gib(grant.reserved.memory_bytes),
                gib(grant.memory_excess())
            ),
        ));
    }

    let disk_floor = policy.disk_floor_bytes(sample.disk_total_bytes);
    if sample.disk_free_bytes < disk_floor {
        out.push(gate(
            Some(AvailabilityFlag::DiskLow),
            QueueReasonCode::DiskLow,
            format!(
                "work volume has {} free, below the {} floor ({}% of {}, at least {})",
                gib(sample.disk_free_bytes),
                gib(disk_floor),
                policy.disk_free_floor_percent,
                gib(sample.disk_total_bytes),
                gib(policy.disk_free_floor_min_bytes)
            ),
        ));
    }

    // Every grant in the journal holds resources, whatever its state.
    if let Some(limit) = policy.max_concurrent_attempts {
        let held = input.grants.len() as u64;
        if held >= u64::from(limit) {
            out.push(gate(
                None,
                QueueReasonCode::MachineConcurrencyLimit,
                format!(
                    "{held} attempt(s) hold resources; policy allows {limit} at a time on this machine"
                ),
            ));
        }
    }

    out
}

/// What the machine can give managed work right now, and whether it is
/// accepting offers at all.
///
/// `ceiling` is the static policy ceiling reduced by what the host shows
/// right now: CPU by foreground use beyond its entitlement, memory and disk
/// to what is charged to the grants plus what could still be granted safely.
/// `free` is `ceiling` minus what the grants are charged.
pub fn budget(input: &Input) -> Budget {
    let static_ceiling = input.policy.managed_ceiling(input.hardware);
    let sums = totals(input.grants);
    let telemetry = fresh_telemetry(input);
    let closed = gates(input, &telemetry);

    let mut flags: Vec<AvailabilityFlag> = closed.iter().filter_map(|g| g.flag).collect();
    flags.sort();
    flags.dedup();
    let admitting = closed.is_empty();
    let mut reasons: Vec<String> = closed.into_iter().map(|g| g.message).collect();

    let Ok(sample) = telemetry else {
        return Budget {
            ceiling: static_ceiling,
            free: ResourceVector::ZERO,
            admitting,
            flags,
            reasons,
        };
    };

    let cpu = cpu_view(input, static_ceiling.cpu_milli, sample.cpu_external_milli());
    if cpu.external > cpu.entitlement {
        if cpu.boosted {
            reasons.push(format!(
                "boost is active: managed CPU stays at the static ceiling of {} while the foreground uses {} against an entitlement of {}",
                cpus(cpu.static_ceiling),
                cpus(cpu.external),
                cpus(cpu.entitlement)
            ));
        } else if cpu.ceiling < cpu.static_ceiling {
            reasons.push(format!(
                "the foreground uses {} against an entitlement of {}; managed CPU ceiling reduced from {} to {}",
                cpus(cpu.external),
                cpus(cpu.entitlement),
                cpus_bare(cpu.static_ceiling),
                cpus(cpu.ceiling)
            ));
        }
    }
    for grant in input.grants {
        let excess = grant.memory_excess();
        if excess > 0 && !is_offender(grant, input.config) {
            reasons.push(format!(
                "attempt {} uses {} of memory, {} over its {} reservation; charged at its real use",
                grant.attempt_id,
                gib(grant.memory_in_use()),
                gib(excess),
                gib(grant.reserved.memory_bytes)
            ));
        }
    }

    let ceiling = ResourceVector {
        cpu_milli: cpu.ceiling,
        memory_bytes: static_ceiling.memory_bytes.min(
            sums.memory_charged
                .saturating_add(memory_grantable(input, sample, &sums)),
        ),
        disk_bytes: static_ceiling.disk_bytes.min(
            sums.reserved
                .disk_bytes
                .saturating_add(disk_grantable(input, sample, &sums)),
        ),
        tokens: static_ceiling.tokens,
    };
    let free = ResourceVector {
        cpu_milli: ceiling.cpu_milli.saturating_sub(sums.reserved.cpu_milli),
        memory_bytes: ceiling.memory_bytes.saturating_sub(sums.memory_charged),
        disk_bytes: ceiling.disk_bytes.saturating_sub(sums.reserved.disk_bytes),
        tokens: ceiling
            .tokens
            .iter()
            .map(|(name, capacity)| {
                (
                    name.clone(),
                    capacity.saturating_sub(sums.reserved.token(name)),
                )
            })
            .collect(),
    };

    Budget {
        ceiling,
        free,
        admitting,
        flags,
        reasons,
    }
}

fn reject(code: QueueReasonCode, message: String) -> Rejection {
    Rejection { code, message }
}

/// A request this machine could never grant, however idle it is.
fn check_ever_possible(
    input: &Input,
    request: &ResourceVector,
    ceiling: &ResourceVector,
) -> Result<(), Rejection> {
    for (name, count) in request.tokens.iter().filter(|(_, count)| **count > 0) {
        match input.policy.tokens.get(name) {
            None => {
                let declared = if input.policy.tokens.is_empty() {
                    "none".to_string()
                } else {
                    input
                        .policy
                        .tokens
                        .keys()
                        .map(|k| format!("`{k}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return Err(reject(
                    QueueReasonCode::MachineTooSmall,
                    format!(
                        "needs {count} of token `{name}`, which this machine does not declare (declared: {declared})"
                    ),
                ));
            }
            Some(capacity) if count > capacity => {
                return Err(reject(
                    QueueReasonCode::MachineTooSmall,
                    format!("needs {count} of token `{name}`; this machine declares {capacity}"),
                ));
            }
            Some(_) => {}
        }
    }
    if request.cpu_milli > ceiling.cpu_milli {
        return Err(reject(
            QueueReasonCode::MachineTooSmall,
            format!(
                "needs {}; this machine's policy ceiling for managed work is {}",
                cpus(request.cpu_milli),
                cpus(ceiling.cpu_milli)
            ),
        ));
    }
    if request.memory_bytes > ceiling.memory_bytes {
        return Err(reject(
            QueueReasonCode::MachineTooSmall,
            format!(
                "needs {} of memory; this machine's policy ceiling for managed work is {}",
                gib(request.memory_bytes),
                gib(ceiling.memory_bytes)
            ),
        ));
    }
    if request.disk_bytes > ceiling.disk_bytes {
        return Err(reject(
            QueueReasonCode::MachineTooSmall,
            format!(
                "needs {} of scratch disk; this machine's policy ceiling for managed work is {}",
                gib(request.disk_bytes),
                gib(ceiling.disk_bytes)
            ),
        ));
    }
    Ok(())
}

/// The worker's final local decision on one offer.
///
/// `Ok` means the request fits both checks of §4 right now, in every
/// dimension, given every grant already held:
///
/// * policy capacity, `Σ max(r_i, u_i) + q ≤ ceiling`, with the CPU ceiling
///   reduced by live foreground use;
/// * immediate safety, `A − F − G − Σ max(r_i − u_i, 0) ≥ q.memory`, and the
///   same shape for scratch disk against the volume's free-space floor;
///
/// and that nothing closes admission altogether: policy, the owner's pause
/// or drain, missing or old telemetry, power, heat, memory pressure, a grant
/// over its reservation, low disk or the concurrency limit. Accepting does
/// not record anything; the caller journals the grant and passes it in
/// `Input::grants` from then on.
pub fn admit(input: &Input, request: &ResourceVector) -> Result<(), Rejection> {
    let static_ceiling = input.policy.managed_ceiling(input.hardware);
    let sums = totals(input.grants);
    let telemetry = fresh_telemetry(input);
    let mut closed = gates(input, &telemetry);

    if let Some(position) = closed
        .iter()
        .position(|g| g.code == QueueReasonCode::BuildsDisabled)
    {
        let disabled = closed.swap_remove(position);
        return Err(reject(disabled.code, disabled.message));
    }
    // Say "never on this machine" before any reason that waiting could fix.
    check_ever_possible(input, request, &static_ceiling)?;
    if let Some(first) = closed.into_iter().next() {
        return Err(reject(first.code, first.message));
    }
    let sample = match telemetry {
        Ok(sample) => sample,
        // Unreachable in practice: missing telemetry is a gate. Refusing
        // again here keeps "never admit blind" independent of that.
        Err(why) => return Err(reject(QueueReasonCode::StaleTelemetry, why)),
    };

    // Policy capacity, CPU.
    let cpu = cpu_view(input, static_ceiling.cpu_milli, sample.cpu_external_milli());
    let reserved_cpu = sums.reserved.cpu_milli;
    let cpu_fits = reserved_cpu
        .checked_add(request.cpu_milli)
        .is_some_and(|need| need <= cpu.ceiling);
    if !cpu_fits {
        let fits_static = reserved_cpu
            .checked_add(request.cpu_milli)
            .is_some_and(|need| need <= cpu.static_ceiling);
        let grantable = cpu.ceiling.saturating_sub(reserved_cpu);
        return Err(if fits_static {
            reject(
                QueueReasonCode::ForegroundProtection,
                format!(
                    "needs {}; {} can be granted now (managed ceiling {} of a static {}, because the foreground uses {} against a {} entitlement; reserved {})",
                    cpus(request.cpu_milli),
                    cpus(grantable),
                    cpus_bare(cpu.ceiling),
                    cpus_bare(cpu.static_ceiling),
                    cpus_bare(cpu.external),
                    cpus_bare(cpu.entitlement),
                    cpus_bare(reserved_cpu)
                ),
            )
        } else {
            reject(
                QueueReasonCode::WaitingForCapacity,
                format!(
                    "needs {}; {} are unreserved (ceiling {}, reserved {})",
                    cpus(request.cpu_milli),
                    cpus(grantable),
                    cpus_bare(cpu.ceiling),
                    cpus_bare(reserved_cpu)
                ),
            )
        });
    }

    // Policy capacity, memory: observed excess is charged at its real use.
    let memory_fits = sums
        .memory_charged
        .checked_add(request.memory_bytes)
        .is_some_and(|need| need <= static_ceiling.memory_bytes);
    if !memory_fits {
        return Err(reject(
            QueueReasonCode::WaitingForCapacity,
            format!(
                "needs {} of memory; {} is unreserved (ceiling {}, charged to grants {})",
                gib(request.memory_bytes),
                gib(static_ceiling
                    .memory_bytes
                    .saturating_sub(sums.memory_charged)),
                gib_bare(static_ceiling.memory_bytes),
                gib_bare(sums.memory_charged)
            ),
        ));
    }

    // Immediate safety, memory.
    let safe = memory_grantable(input, sample, &sums);
    if request.memory_bytes > safe {
        return Err(reject(
            QueueReasonCode::ForegroundProtection,
            format!(
                "needs {}; {} can be granted safely now (available {}, floor {}, guard {}, promised {})",
                gib(request.memory_bytes),
                gib(safe),
                gib_bare(sample.memory_available_bytes),
                gib_bare(input.policy.available_memory_floor_bytes),
                gib_bare(input.config.memory_guard_bytes),
                gib_bare(sums.memory_promised)
            ),
        ));
    }

    // Policy capacity, disk.
    let disk_fits = sums
        .reserved
        .disk_bytes
        .checked_add(request.disk_bytes)
        .is_some_and(|need| need <= static_ceiling.disk_bytes);
    if !disk_fits {
        return Err(reject(
            QueueReasonCode::WaitingForCapacity,
            format!(
                "needs {} of scratch disk; {} is unreserved (ceiling {}, reserved {})",
                gib(request.disk_bytes),
                gib(static_ceiling
                    .disk_bytes
                    .saturating_sub(sums.reserved.disk_bytes)),
                gib_bare(static_ceiling.disk_bytes),
                gib_bare(sums.reserved.disk_bytes)
            ),
        ));
    }

    // Live disk: free space, the floor, and promises not yet written.
    let disk = disk_grantable(input, sample, &sums);
    if request.disk_bytes > disk {
        return Err(reject(
            QueueReasonCode::DiskLow,
            format!(
                "needs {} of scratch disk; {} can be granted now (free {}, floor {}, promised {})",
                gib(request.disk_bytes),
                gib(disk),
                gib_bare(sample.disk_free_bytes),
                gib_bare(input.policy.disk_floor_bytes(sample.disk_total_bytes)),
                gib_bare(sums.disk_promised)
            ),
        ));
    }

    // Policy capacity, tokens. Undeclared tokens were refused above.
    for (name, count) in request.tokens.iter().filter(|(_, count)| **count > 0) {
        let capacity = input.policy.tokens.get(name).copied().unwrap_or(0);
        let held = sums.reserved.token(name);
        let fits = held
            .checked_add(*count)
            .is_some_and(|need| need <= capacity);
        if !fits {
            return Err(reject(
                QueueReasonCode::WaitingForCapacity,
                format!(
                    "needs {count} of token `{name}`; {} free (declared {capacity}, reserved {held})",
                    capacity.saturating_sub(held)
                ),
            ));
        }
    }

    Ok(())
}
