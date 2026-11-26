//! Pressure response: hysteresis, CPU limits and the eviction choice.

use crate::admission::{cpu_view, fresh_telemetry, is_offender};
use crate::format::{gib, secs};
use crate::{Action, Grant, Input, ProtectConfig};
use bella_core::job::{Priority, RetryClass};
use bella_core::machine::{PressureLevel, Telemetry};
use bella_core::{AttemptId, UnixMillis};
use std::cmp::Reverse;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// Samples closer together than this are merged into one record, keeping
/// the worse reading, so a fast caller cannot grow the record without bound.
const MIN_SAMPLE_SPACING: Duration = Duration::from_millis(100);

/// One recorded observation, or several merged ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    /// Timestamp of the first observation in this record.
    from: UnixMillis,
    /// Timestamp of the last observation in this record.
    at: UnixMillis,
    pressure: PressureLevel,
    /// Available memory was below the policy floor.
    below_floor: bool,
    cpu_external_milli: u64,
}

/// Rolling record of memory pressure, floor breaches and foreground CPU
/// use, plus what the worker has applied so far.
///
/// The worker owns one `History` for the life of its protection loop. It
/// changes only through [`History::observe`] (one telemetry sample) and
/// [`History::record`] (the actions that were applied); [`actions`] reads it.
/// Durations are measured between sample timestamps, so a condition counts
/// as sustained only on the evidence of samples that actually cover that
/// time: a repeated sample adds nothing, and a hole longer than
/// `telemetry_max_age` starts the count again.
///
/// A fresh `History` (after a worker restart) has admission open and no
/// limits applied; every grace period starts over.
#[derive(Clone, Debug, Default)]
pub struct History {
    samples: VecDeque<Sample>,
    admission_stopped: Option<String>,
    /// Victims of recorded evictions that still hold resources.
    evicting: Vec<AttemptId>,
    last_eviction_at: Option<UnixMillis>,
    /// Limits recorded as applied. A started grant with no entry runs at
    /// its reservation.
    cpu_limits: BTreeMap<AttemptId, u64>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the telemetry sample in `input`. Call once per protection
    /// interval, before [`actions`].
    ///
    /// A sample that is missing, not fresh or already recorded adds
    /// nothing. A sample older than the newest recorded one means the wall
    /// clock stepped back: the record restarts from it. State kept for
    /// attempts that no longer hold a grant is dropped.
    pub fn observe(&mut self, input: &Input) {
        let held = |id: &AttemptId| input.grants.iter().any(|g| &g.attempt_id == id);
        self.evicting.retain(&held);
        self.cpu_limits.retain(|id, _| held(id));

        let Ok(telemetry) = fresh_telemetry(input) else {
            return;
        };
        let sample = Sample {
            from: telemetry.sampled_at,
            at: telemetry.sampled_at,
            pressure: telemetry.memory_pressure,
            below_floor: telemetry.memory_available_bytes
                < input.policy.available_memory_floor_bytes,
            cpu_external_milli: telemetry.cpu_external_milli(),
        };
        match self.samples.back().copied() {
            Some(last) if sample.at == last.at => return,
            Some(last) if sample.at < last.at => {
                self.samples.clear();
                self.samples.push_back(sample);
            }
            Some(last) if sample.at.since(last.from) < MIN_SAMPLE_SPACING => {
                if let Some(slot) = self.samples.back_mut() {
                    slot.at = sample.at;
                    slot.pressure = slot.pressure.max(sample.pressure);
                    slot.below_floor |= sample.below_floor;
                    slot.cpu_external_milli =
                        slot.cpu_external_milli.max(sample.cpu_external_milli);
                }
            }
            _ => self.samples.push_back(sample),
        }

        // Keep twice the longest period any decision looks back over.
        let config = input.config;
        let longest = [
            config.pressure_warning_grace,
            config.pressure_critical_grace,
            config.floor_breach_grace,
            config.recovery_period,
            config.cpu_restore_delay,
        ]
        .into_iter()
        .max()
        .unwrap_or_default();
        let keep = longest
            .saturating_mul(2)
            .saturating_add(config.telemetry_max_age);
        let cutoff = sample.at.minus(keep);
        while self.samples.front().is_some_and(|s| s.at < cutoff) {
            self.samples.pop_front();
        }
    }

    /// Record actions the worker has applied. Until a
    /// [`Action::StopAdmission`], limit or eviction is recorded, [`actions`]
    /// keeps proposing it.
    pub fn record(&mut self, applied: &[Action], now: UnixMillis) {
        for action in applied {
            match action {
                Action::StopAdmission { reason } => {
                    self.admission_stopped = Some(reason.clone());
                }
                Action::ResumeAdmission => self.admission_stopped = None,
                Action::SetCpuLimit {
                    attempt_id,
                    cpu_milli,
                } => {
                    self.cpu_limits.insert(attempt_id.clone(), *cpu_milli);
                }
                Action::Evict { attempt_id, .. } => {
                    if !self.evicting.contains(attempt_id) {
                        self.evicting.push(attempt_id.clone());
                    }
                    self.last_eviction_at = Some(now);
                }
            }
        }
    }

    /// The reason admission is stopped, between a recorded
    /// [`Action::StopAdmission`] and the next [`Action::ResumeAdmission`].
    /// While this is `Some` the worker refuses every offer.
    pub fn admission_stopped(&self) -> Option<&str> {
        self.admission_stopped.as_deref()
    }

    /// An eviction of this attempt was recorded and it still holds a grant.
    pub fn is_evicting(&self, attempt_id: &AttemptId) -> bool {
        self.evicting.contains(attempt_id)
    }

    /// The CPU limit recorded as applied to this attempt, if any.
    pub fn cpu_limit(&self, attempt_id: &AttemptId) -> Option<u64> {
        self.cpu_limits.get(attempt_id).copied()
    }

    /// How long `holds` has been true of every sample up to the newest
    /// one, with no hole longer than `max_gap`. `None` when it is not true
    /// of the newest sample.
    fn run(&self, max_gap: Duration, holds: impl Fn(&Sample) -> bool) -> Option<Duration> {
        let newest = self.samples.back()?;
        if !holds(newest) {
            return None;
        }
        let mut start = newest;
        for sample in self.samples.iter().rev().skip(1) {
            if !holds(sample) || start.from.since(sample.at) > max_gap {
                break;
            }
            start = sample;
        }
        Some(newest.at.since(start.from))
    }

    /// Highest foreground CPU use among samples newer than `window`.
    fn peak_external(&self, window: Duration) -> u64 {
        let Some(newest) = self.samples.back() else {
            return 0;
        };
        self.samples
            .iter()
            .rev()
            .take_while(|s| s.at.plus(window) > newest.at)
            .map(|s| s.cpu_external_milli)
            .max()
            .unwrap_or(0)
    }
}

fn elevated(sample: &Sample) -> bool {
    sample.pressure >= PressureLevel::Warning
}

fn critical(sample: &Sample) -> bool {
    sample.pressure >= PressureLevel::Critical
}

fn below_floor(sample: &Sample) -> bool {
    sample.below_floor
}

/// `Unknown` pressure is not elevated: on a host that reports no pressure
/// level, the available-memory floor is the signal.
fn calm(sample: &Sample) -> bool {
    !elevated(sample) && !sample.below_floor
}

/// What the worker should do now to protect the host. Call after
/// [`History::observe`]; pass what was applied to [`History::record`].
///
/// Without a fresh sample nothing is proposed: no eviction happens on old
/// evidence. Otherwise, in this order:
///
/// * [`Action::StopAdmission`] once memory pressure has stayed at `Warning`
///   or above for `pressure_warning_grace`, `Critical` for
///   `pressure_critical_grace`, or available memory below the floor for
///   `floor_breach_grace`.
/// * [`Action::SetCpuLimit`] for started grants whose limit should change.
///   When foreground CPU use exceeds its entitlement the reduced managed
///   ceiling is shared in proportion to the reservations, never below
///   `min_cpu_milli_per_attempt`, so the limits can add up to more than the
///   ceiling when that minimum binds. While admission is stopped each grant
///   is also held to `pressure_cpu_percent` of its reservation. Limits drop
///   at once and rise only after `cpu_restore_delay` of quieter foreground.
///   A boost suspends the foreground reduction, not the pressure one.
/// * At most one [`Action::Evict`], and none within `eviction_cooldown` of
///   the previous one, when pressure has been `Critical` or memory below
///   the floor for its grace period. A grant over its memory reservation is
///   evicted without a grace period as soon as pressure is `Warning` or
///   above or memory is below the floor.
/// * [`Action::ResumeAdmission`] once pressure has been normal and memory
///   above the floor for `recovery_period`.
///
/// The victim is the first of the started grants not already being evicted,
/// ordered by: over its reservation first, then lowest priority, then
/// `Pure` before `IsolatedSideEffects`, then most recently started. An
/// `ExternalSideEffects` grant is never chosen unless it is the only grant
/// over its reservation. A grant that has not started is never chosen:
/// it holds no memory to give back.
pub fn actions(input: &Input, history: &History) -> Vec<Action> {
    let mut out = Vec::new();
    let Ok(telemetry) = fresh_telemetry(input) else {
        return out;
    };
    let config = input.config;
    let floor = input.policy.available_memory_floor_bytes;
    let available = telemetry.memory_available_bytes;

    // Sustained conditions need the record to end at this very sample.
    let synced = history
        .samples
        .back()
        .is_some_and(|s| s.at == telemetry.sampled_at);
    let sustained = |grace: Duration, holds: fn(&Sample) -> bool| {
        if !synced {
            return None;
        }
        history
            .run(config.telemetry_max_age, holds)
            .filter(|run| *run >= grace)
    };
    let critical_for = sustained(config.pressure_critical_grace, critical);
    let breach_for = sustained(config.floor_breach_grace, below_floor);
    let warning_for = sustained(config.pressure_warning_grace, elevated);
    let calm_for = sustained(config.recovery_period, calm);

    // Conditions that call for an eviction.
    let relief = if let Some(run) = critical_for {
        Some(format!(
            "memory pressure has been critical for {} (grace {}), available {}",
            secs(run),
            secs(config.pressure_critical_grace),
            gib(available)
        ))
    } else {
        breach_for.map(|run| {
            format!(
                "available memory {} has been below the {} floor for {} (grace {})",
                gib(available),
                gib(floor),
                secs(run),
                secs(config.floor_breach_grace)
            )
        })
    };
    let distress = relief.clone().or_else(|| {
        warning_for.map(|run| {
            format!(
                "memory pressure has been at warning or above for {} (grace {}), available {}",
                secs(run),
                secs(config.pressure_warning_grace),
                gib(available)
            )
        })
    });

    let stopped = history.admission_stopped.is_some();
    if let (false, Some(reason)) = (stopped, &distress) {
        out.push(Action::StopAdmission {
            reason: reason.clone(),
        });
    }
    let resume = stopped && distress.is_none() && calm_for.is_some();

    let throttled = distress.is_some() || (stopped && !resume);
    for plan in cpu_plan(input, telemetry, history, synced, throttled) {
        let reserved = plan.grant.reserved.cpu_milli;
        let applied = history
            .cpu_limit(&plan.grant.attempt_id)
            .unwrap_or(reserved);
        if plan.limit == applied {
            continue;
        }
        let at_bound = plan.limit == reserved || plan.limit == plan.lowest;
        if at_bound || plan.limit.abs_diff(applied) >= config.cpu_limit_step_milli {
            out.push(Action::SetCpuLimit {
                attempt_id: plan.grant.attempt_id.clone(),
                cpu_milli: plan.limit,
            });
        }
    }

    let cooled = history
        .last_eviction_at
        .is_none_or(|at| at > input.now || input.now.since(at) >= config.eviction_cooldown);
    let pressured_now = if telemetry.memory_pressure >= PressureLevel::Warning {
        Some(format!(
            "memory pressure is {}, available {}",
            pressure_name(telemetry.memory_pressure),
            gib(available)
        ))
    } else if available < floor {
        Some(format!(
            "available memory {} is below the {} floor",
            gib(available),
            gib(floor)
        ))
    } else {
        None
    };
    if cooled
        && let Some(victim) = choose_victim(input, history, relief.is_some())
        && let Some(cause) = relief.or(pressured_now)
    {
        out.push(Action::Evict {
            attempt_id: victim.attempt_id.clone(),
            reason: format!("{cause}; evicting {}", describe_victim(victim, config)),
        });
    }

    if resume {
        out.push(Action::ResumeAdmission);
    }
    out
}

/// The name `docs/architecture.md` §6.10 gives [`actions`].
pub fn pressure_actions(input: &Input, history: &History) -> Vec<Action> {
    actions(input, history)
}

struct CpuPlan<'a> {
    grant: &'a Grant,
    /// The limit the grant should run at.
    limit: u64,
    /// The lowest limit this grant may be given.
    lowest: u64,
}

/// The CPU limit each started grant should have.
fn cpu_plan<'a>(
    input: &Input<'a>,
    telemetry: &Telemetry,
    history: &History,
    synced: bool,
    throttled: bool,
) -> Vec<CpuPlan<'a>> {
    let config = input.config;
    let static_cpu = input.policy.managed_ceiling(input.hardware).cpu_milli;
    let mut external = telemetry.cpu_external_milli();
    if synced {
        external = external.max(history.peak_external(config.cpu_restore_delay));
    }
    let ceiling = u128::from(cpu_view(input, static_cpu, external).ceiling);
    let percent = u128::from(config.pressure_cpu_percent.min(100));

    let shares: Vec<(&Grant, u128)> = input
        .grants
        .iter()
        .filter(|g| g.started)
        .map(|g| {
            let reserved = u128::from(g.reserved.cpu_milli);
            let share = if throttled {
                reserved * percent / 100
            } else {
                reserved
            };
            (g, share)
        })
        .collect();
    let wanted: u128 = shares.iter().map(|(_, share)| share).sum();

    shares
        .into_iter()
        .map(|(grant, share)| {
            let reserved = grant.reserved.cpu_milli;
            let lowest = config.min_cpu_milli_per_attempt.min(reserved);
            let fair = if wanted > ceiling {
                ceiling * share / wanted
            } else {
                share
            };
            let limit = u64::try_from(fair)
                .unwrap_or(reserved)
                .clamp(lowest, reserved);
            CpuPlan {
                grant,
                limit,
                lowest,
            }
        })
        .collect()
}

/// The grant to evict, if any may be.
///
/// With `relief` any eligible started grant can be chosen; without it only
/// one that is over its own reservation.
fn choose_victim<'a>(input: &Input<'a>, history: &History, relief: bool) -> Option<&'a Grant> {
    let config = input.config;
    let live: Vec<&Grant> = input
        .grants
        .iter()
        .filter(|g| g.started && !history.is_evicting(&g.attempt_id))
        .collect();
    let offenders = live.iter().filter(|g| is_offender(g, config)).count();
    live.into_iter()
        .filter(|g| {
            let offender = is_offender(g, config);
            let external = g.retry_class == RetryClass::ExternalSideEffects;
            (!external || (offender && offenders == 1)) && (relief || offender)
        })
        .min_by(|a, b| victim_rank(a, config).cmp(&victim_rank(b, config)))
}

/// Sort key of a candidate victim; the smallest is evicted.
fn victim_rank<'a>(
    grant: &'a Grant,
    config: &ProtectConfig,
) -> (
    bool,
    Priority,
    u8,
    Reverse<UnixMillis>,
    Reverse<&'a AttemptId>,
) {
    let retry = match grant.retry_class {
        RetryClass::Pure => 0,
        RetryClass::IsolatedSideEffects => 1,
        RetryClass::ExternalSideEffects => 2,
    };
    (
        !is_offender(grant, config),
        grant.priority,
        retry,
        // An unknown start time counts as the oldest.
        Reverse(grant.started_at.unwrap_or(UnixMillis(0))),
        Reverse(&grant.attempt_id),
    )
}

fn describe_victim(grant: &Grant, config: &ProtectConfig) -> String {
    let priority = match grant.priority {
        Priority::Maintenance => "maintenance",
        Priority::Background => "background",
        Priority::Normal => "normal",
        Priority::Interactive => "interactive",
        Priority::Urgent => "urgent",
    };
    let retry = match grant.retry_class {
        RetryClass::Pure => "pure",
        RetryClass::IsolatedSideEffects => "isolated side effects",
        RetryClass::ExternalSideEffects => "external side effects",
    };
    let usage = if is_offender(grant, config) {
        format!(
            "uses {} of memory against a {} reservation ({} over)",
            gib(grant.memory_in_use()),
            gib(grant.reserved.memory_bytes),
            gib(grant.memory_excess())
        )
    } else {
        format!(
            "uses {} of memory within a {} reservation",
            gib(grant.memory_in_use()),
            gib(grant.reserved.memory_bytes)
        )
    };
    format!(
        "attempt {} (priority {priority}, {retry}), which {usage}",
        grant.attempt_id
    )
}

fn pressure_name(level: PressureLevel) -> &'static str {
    match level {
        PressureLevel::Unknown => "unknown",
        PressureLevel::Normal => "normal",
        PressureLevel::Warning => "warning",
        PressureLevel::Critical => "critical",
    }
}
