//! Live telemetry of the local host.

use crate::osparse::{self, CpuTimes};
use crate::sys::{self, Origin};
use bella_core::machine::{MeasurementQuality, PowerState, Telemetry, ThermalState};
use bella_core::{Clock, SystemClock};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the worker's containment scopes are using, supplied to
/// [`Sampler::sample`] because only the worker knows its scopes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManagedUsage {
    /// CPU time consumed by all managed scopes since the worker started,
    /// in milliseconds. It must only grow: keep the time of finished scopes
    /// in the total. A value lower than the previous one is treated as no
    /// use for that interval.
    pub cpu_time_ms_total: u64,
    /// Memory currently charged to managed scopes.
    pub memory_bytes: u64,
}

/// Where one telemetry counter came from in a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CounterSource {
    /// Read from a counter the OS reports directly.
    Measured,
    /// Derived by a documented rule of this crate, not reported by the OS.
    Estimated,
    /// Could not be read; the telemetry field holds its zero or unknown
    /// value.
    Unavailable,
}

impl From<Origin> for CounterSource {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Measured => Self::Measured,
            Origin::Estimated => Self::Estimated,
            Origin::Unavailable => Self::Unavailable,
        }
    }
}

/// Per-counter provenance of the most recent sample. This is the detail
/// behind the single [`MeasurementQuality`] in [`Telemetry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CounterReport {
    /// `cpu_used_milli`.
    pub cpu: CounterSource,
    /// `cpu_managed_milli`. `Unavailable` on a sampler's first sample,
    /// which has no earlier total to compare with.
    pub cpu_managed: CounterSource,
    /// `memory_total_bytes` and `memory_available_bytes`.
    pub memory: CounterSource,
    /// `swap_used_bytes`.
    pub swap: CounterSource,
    /// `memory_pressure`.
    pub pressure: CounterSource,
    /// `disk_total_bytes` and `disk_free_bytes`.
    pub disk: CounterSource,
    /// `power`. `Measured` also when the OS says there is no battery.
    pub power: CounterSource,
    /// `thermal`. `Unavailable` where the OS offers no thermal state, in
    /// which case the field is `ThermalState::Unknown`.
    pub thermal: CounterSource,
}

impl CounterReport {
    /// The overall quality reported in [`Telemetry::quality`].
    ///
    /// `Good` when CPU, managed CPU, memory, swap, memory pressure and disk
    /// were all measured. `Unknown` when none of them could be read.
    /// `Degraded` otherwise: at least one is estimated or missing.
    ///
    /// Power and thermal state do not count, because their own values
    /// (`None`, `Unknown`) already say when nothing is known.
    pub fn quality(&self) -> MeasurementQuality {
        let counted = [
            self.cpu,
            self.cpu_managed,
            self.memory,
            self.swap,
            self.pressure,
            self.disk,
        ];
        if counted.iter().all(|&s| s == CounterSource::Measured) {
            MeasurementQuality::Good
        } else if counted.iter().all(|&s| s == CounterSource::Unavailable) {
            MeasurementQuality::Unknown
        } else {
            MeasurementQuality::Degraded
        }
    }
}

/// Readings that are cached between samples because they are slow to get
/// and change slowly.
#[derive(Clone, Copy)]
struct SlowReadings {
    taken: Instant,
    power: Option<PowerState>,
    thermal: Option<ThermalState>,
}

/// Samples what the local host is doing.
///
/// A sampler is cheap enough to call once a second (see the crate
/// documentation for measurements): per sample it reads a handful of OS
/// counters and starts no process, except that on macOS the power and
/// thermal state come from `pmset`, which runs at most once every
/// [`Sampler::SLOW_REFRESH`].
pub struct Sampler {
    work_dir: PathBuf,
    clock: Arc<dyn Clock>,
    state: sys::os::SamplerState,
    logical_cpus: u32,
    previous_cpu: Option<CpuTimes>,
    previous_at: Instant,
    previous_managed_ms: Option<u64>,
    slow: Option<SlowReadings>,
    last_report: CounterReport,
}

impl Sampler {
    /// The shortest interval CPU use is averaged over. A [`sample`] call
    /// made sooner after the previous one (or after [`new`]) waits for the
    /// rest of this interval first, so callers at 1 Hz never wait.
    ///
    /// [`sample`]: Sampler::sample
    /// [`new`]: Sampler::new
    pub const MIN_CPU_WINDOW: Duration = Duration::from_millis(200);

    /// How long power and thermal readings are reused before they are read
    /// again.
    pub const SLOW_REFRESH: Duration = Duration::from_secs(10);

    /// A sampler for the host, reporting disk space of the volume that
    /// holds `work_dir`. The directory does not have to exist yet; its
    /// nearest existing ancestor is used.
    ///
    /// The CPU baseline is taken here, so the first [`sample`](Self::sample)
    /// reports CPU use since this call.
    pub fn new(work_dir: &Path) -> Self {
        Self::with_clock(work_dir, Arc::new(SystemClock))
    }

    /// Like [`new`](Self::new), with the clock that stamps
    /// [`Telemetry::sampled_at`].
    pub fn with_clock(work_dir: &Path, clock: Arc<dyn Clock>) -> Self {
        let mut state = sys::os::SamplerState::new();
        let previous_cpu = sys::os::cpu_times(&mut state);
        let unavailable = CounterSource::Unavailable;
        Self {
            work_dir: std::path::absolute(work_dir).unwrap_or_else(|_| work_dir.to_path_buf()),
            clock,
            state,
            logical_cpus: sys::os::logical_cpus().max(1),
            previous_cpu,
            previous_at: Instant::now(),
            previous_managed_ms: None,
            slow: None,
            last_report: CounterReport {
                cpu: unavailable,
                cpu_managed: unavailable,
                memory: unavailable,
                swap: unavailable,
                pressure: unavailable,
                disk: unavailable,
                power: unavailable,
                thermal: unavailable,
            },
        }
    }

    /// Take one sample.
    ///
    /// * `cpu_used_milli` is whole-host CPU use averaged since the previous
    ///   call (since construction for the first call), over at least
    ///   [`MIN_CPU_WINDOW`](Self::MIN_CPU_WINDOW); a call made sooner
    ///   blocks for the remainder.
    /// * `cpu_managed_milli` is the growth of `managed.cpu_time_ms_total`
    ///   over the wall time since the previous call. The first call has
    ///   nothing to compare with: it reports 0 and the sample's quality is
    ///   `Degraded`.
    /// * `memory_available_bytes` is the OS's own notion of memory it can
    ///   hand out without swapping: `MemAvailable` on Linux; free,
    ///   speculative, inactive and purgeable pages on macOS; available
    ///   physical memory on Windows.
    /// * `memory_pressure` is the macOS pressure level, Linux PSI (see
    ///   [`osparse::pressure_from_psi`]), or on Windows the heuristic in
    ///   [`osparse::windows_pressure`], which makes every Windows sample
    ///   `Degraded`.
    /// * `power` is `None`/`None` on a machine without a battery, on every
    ///   OS. `thermal` is `Unknown` on Linux and Windows.
    /// * `quality` follows [`CounterReport::quality`]; the per-counter
    ///   detail is available from [`last_counters`](Self::last_counters).
    ///
    /// A counter that cannot be read leaves its field at zero (or
    /// `Unknown`) and lowers the quality; sampling itself never fails.
    pub fn sample(&mut self, managed: ManagedUsage) -> Telemetry {
        let since_previous = self.previous_at.elapsed();
        if since_previous < Self::MIN_CPU_WINDOW {
            std::thread::sleep(Self::MIN_CPU_WINDOW - since_previous);
        }
        self.sample_at(Instant::now(), managed)
    }

    /// Provenance of each counter in the most recent sample. Before the
    /// first sample everything is `Unavailable`.
    pub fn last_counters(&self) -> CounterReport {
        self.last_report
    }

    /// Take a sample as of `now`, without waiting.
    fn sample_at(&mut self, now: Instant, managed: ManagedUsage) -> Telemetry {
        let wall = now.saturating_duration_since(self.previous_at);

        let current_cpu = sys::os::cpu_times(&mut self.state);
        let cpu_used = match (self.previous_cpu, current_cpu) {
            (Some(previous), Some(current)) => {
                osparse::cpu_used_milli(previous, current, self.logical_cpus)
            }
            _ => None,
        };
        if current_cpu.is_some() {
            self.previous_cpu = current_cpu;
        }

        let cpu_managed = self.previous_managed_ms.and_then(|previous| {
            osparse::managed_cpu_milli(
                previous,
                managed.cpu_time_ms_total,
                wall.as_millis().min(u128::from(u64::MAX)) as u64,
                self.logical_cpus,
            )
        });
        self.previous_managed_ms = Some(managed.cpu_time_ms_total);
        self.previous_at = now;

        let memory = sys::os::memory();
        let (pressure, pressure_origin) = sys::os::pressure(&self.state, memory.as_ref());
        let disk = self.disk_usage();
        let slow = self.slow_readings(now);
        let thermal = [sys::os::thermal_fast(&self.state), slow.thermal]
            .into_iter()
            .flatten()
            .max();

        let measured_if = |present: bool| {
            if present {
                CounterSource::Measured
            } else {
                CounterSource::Unavailable
            }
        };
        self.last_report = CounterReport {
            cpu: measured_if(cpu_used.is_some()),
            cpu_managed: measured_if(cpu_managed.is_some()),
            memory: match &memory {
                Some(reading) if reading.available_exact => CounterSource::Measured,
                Some(_) => CounterSource::Estimated,
                None => CounterSource::Unavailable,
            },
            swap: measured_if(memory.is_some_and(|reading| reading.swap_used.is_some())),
            pressure: pressure_origin.into(),
            disk: measured_if(disk.is_some()),
            power: measured_if(slow.power.is_some()),
            thermal: measured_if(thermal.is_some()),
        };

        let (disk_total, disk_free) = disk.unwrap_or((0, 0));
        Telemetry {
            sampled_at: self.clock.now(),
            cpu_used_milli: cpu_used.unwrap_or(0),
            cpu_managed_milli: cpu_managed.unwrap_or(0),
            memory_total_bytes: memory.map_or(0, |reading| reading.total),
            memory_available_bytes: memory.map_or(0, |reading| reading.available),
            memory_managed_bytes: managed.memory_bytes,
            swap_used_bytes: memory.and_then(|reading| reading.swap_used).unwrap_or(0),
            memory_pressure: pressure,
            disk_total_bytes: disk_total,
            disk_free_bytes: disk_free.min(disk_total),
            power: slow.power.unwrap_or_default(),
            thermal: thermal.unwrap_or(ThermalState::Unknown),
            quality: self.last_report.quality(),
        }
    }

    /// Total and free bytes of the volume holding the work directory, or
    /// its nearest ancestor that exists.
    fn disk_usage(&self) -> Option<(u64, u64)> {
        self.work_dir.ancestors().find_map(sys::os::disk_usage)
    }

    fn slow_readings(&mut self, now: Instant) -> SlowReadings {
        match self.slow {
            Some(cached) if now.saturating_duration_since(cached.taken) < Self::SLOW_REFRESH => {
                cached
            }
            _ => {
                let fresh = SlowReadings {
                    taken: now,
                    power: sys::os::power(),
                    thermal: sys::os::thermal_slow(),
                };
                self.slow = Some(fresh);
                fresh
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::machine::PressureLevel;
    use bella_core::{ManualClock, UnixMillis};

    fn all(source: CounterSource) -> CounterReport {
        CounterReport {
            cpu: source,
            cpu_managed: source,
            memory: source,
            swap: source,
            pressure: source,
            disk: source,
            power: source,
            thermal: source,
        }
    }

    #[test]
    fn quality_is_good_only_when_every_resource_counter_was_measured() {
        let good = all(CounterSource::Measured);
        assert_eq!(good.quality(), MeasurementQuality::Good);
        // Power and thermal carry their own "unknown" and do not count.
        let no_sensors = CounterReport {
            power: CounterSource::Unavailable,
            thermal: CounterSource::Unavailable,
            ..good
        };
        assert_eq!(no_sensors.quality(), MeasurementQuality::Good);
        let heuristic = CounterReport {
            pressure: CounterSource::Estimated,
            ..good
        };
        assert_eq!(heuristic.quality(), MeasurementQuality::Degraded);
        let missing = CounterReport {
            swap: CounterSource::Unavailable,
            ..good
        };
        assert_eq!(missing.quality(), MeasurementQuality::Degraded);
        assert_eq!(
            all(CounterSource::Unavailable).quality(),
            MeasurementQuality::Unknown
        );
    }

    #[test]
    fn first_sample_has_no_managed_cpu_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = Sampler::new(dir.path());
        let start = Instant::now() + Duration::from_secs(1);
        let first = sampler.sample_at(
            start,
            ManagedUsage {
                cpu_time_ms_total: 50_000,
                memory_bytes: 123,
            },
        );
        assert_eq!(first.cpu_managed_milli, 0);
        assert_eq!(
            sampler.last_counters().cpu_managed,
            CounterSource::Unavailable
        );
        assert_eq!(first.quality, MeasurementQuality::Degraded);
        assert_eq!(first.memory_managed_bytes, 123);
    }

    #[test]
    fn managed_cpu_is_the_growth_over_wall_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = Sampler::new(dir.path());
        let cpus = u64::from(sampler.logical_cpus);
        let start = Instant::now() + Duration::from_secs(1);
        let usage = |cpu_time_ms_total| ManagedUsage {
            cpu_time_ms_total,
            memory_bytes: 0,
        };
        sampler.sample_at(start, usage(10_000));
        // 500 ms of CPU in two seconds: a quarter of one CPU.
        let second = sampler.sample_at(start + Duration::from_secs(2), usage(10_500));
        assert_eq!(second.cpu_managed_milli, 250);
        assert_eq!(sampler.last_counters().cpu_managed, CounterSource::Measured);
        // A total that went backwards is no use, not a huge number.
        let third = sampler.sample_at(start + Duration::from_secs(3), usage(9_000));
        assert_eq!(third.cpu_managed_milli, 0);
        // And never more than the machine has.
        let fourth = sampler.sample_at(start + Duration::from_secs(4), usage(9_000_000));
        assert_eq!(fourth.cpu_managed_milli, cpus * 1000);
    }

    #[test]
    fn samples_are_stamped_by_the_injected_clock() {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::at(UnixMillis(1_700_000_000_000));
        let mut sampler = Sampler::with_clock(dir.path(), Arc::new(clock.clone()));
        let start = Instant::now() + Duration::from_secs(1);
        let first = sampler.sample_at(start, ManagedUsage::default());
        assert_eq!(first.sampled_at, UnixMillis(1_700_000_000_000));
        clock.advance(Duration::from_secs(1));
        let second = sampler.sample_at(start + Duration::from_secs(1), ManagedUsage::default());
        assert_eq!(second.sampled_at, UnixMillis(1_700_000_001_000));
    }

    #[test]
    fn slow_readings_are_reused_until_they_expire() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = Sampler::new(dir.path());
        let start = Instant::now() + Duration::from_secs(1);
        sampler.sample_at(start, ManagedUsage::default());
        let taken = sampler.slow.map(|slow| slow.taken);
        assert_eq!(taken, Some(start));
        sampler.sample_at(start + Duration::from_secs(9), ManagedUsage::default());
        assert_eq!(sampler.slow.map(|slow| slow.taken), Some(start));
        let later = start + Sampler::SLOW_REFRESH;
        sampler.sample_at(later, ManagedUsage::default());
        assert_eq!(sampler.slow.map(|slow| slow.taken), Some(later));
    }

    #[test]
    fn disk_of_a_missing_work_dir_is_its_nearest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let existing = Sampler::new(dir.path())
            .disk_usage()
            .expect("volume is readable");
        let missing = dir.path().join("not").join("created").join("yet");
        let nested = Sampler::new(&missing)
            .disk_usage()
            .expect("ancestor is readable");
        assert_eq!(nested.0, existing.0);
        assert!(existing.0 > 0 && existing.1 <= existing.0);
    }

    /// Live smoke test: counters of the machine running the tests are sane.
    #[test]
    fn live_sample_is_sane() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = Sampler::new(dir.path());
        let cpus = u64::from(sampler.logical_cpus);
        assert!(cpus > 0);
        let first = sampler.sample(ManagedUsage::default());
        let second = sampler.sample(ManagedUsage {
            cpu_time_ms_total: 10,
            memory_bytes: 1 << 20,
        });
        for sample in [&first, &second] {
            assert!(sample.sampled_at.0 > 1_600_000_000_000);
            assert!(sample.cpu_used_milli <= cpus * 1000);
            assert!(sample.memory_total_bytes > 0);
            assert!(sample.memory_available_bytes > 0);
            assert!(sample.memory_total_bytes > sample.memory_available_bytes);
            assert!(sample.disk_total_bytes > 0);
            assert!(sample.disk_total_bytes >= sample.disk_free_bytes);
            assert!(sample.power.battery_percent.is_none_or(|p| p <= 100));
            assert_ne!(sample.quality, MeasurementQuality::Unknown);
        }
        let counters = sampler.last_counters();
        assert_eq!(counters.cpu, CounterSource::Measured);
        assert_eq!(counters.cpu_managed, CounterSource::Measured);
        assert_eq!(counters.memory, CounterSource::Measured);
        assert_eq!(counters.disk, CounterSource::Measured);
        assert_eq!(second.memory_managed_bytes, 1 << 20);
        assert_eq!(second.quality, counters.quality());
        // A level the OS really reported is never `Unknown`.
        if counters.pressure == CounterSource::Measured {
            assert_ne!(second.memory_pressure, PressureLevel::Unknown);
        }
        // Windows pressure is a heuristic, so its samples are never `Good`.
        if cfg!(windows) {
            assert_eq!(counters.pressure, CounterSource::Estimated);
            assert_eq!(second.quality, MeasurementQuality::Degraded);
        }
        // macOS reads pressure and thermal state from the OS.
        if cfg!(target_os = "macos") {
            assert_eq!(counters.pressure, CounterSource::Measured);
            assert_ne!(second.thermal, ThermalState::Unknown);
            assert_eq!(second.quality, MeasurementQuality::Good);
        }
    }

    /// A busy thread shows up as host CPU use.
    #[test]
    fn live_cpu_use_reflects_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = Sampler::new(dir.path());
        sampler.sample(ManagedUsage::default());
        let until = Instant::now() + Duration::from_millis(400);
        let mut spins = 0u64;
        while Instant::now() < until {
            spins = std::hint::black_box(spins.wrapping_add(1));
        }
        let loaded = sampler.sample(ManagedUsage::default());
        // One thread spinning for the whole interval is a full CPU; allow
        // for coarse OS tick accounting and scheduling on a busy host.
        assert!(
            loaded.cpu_used_milli >= 300,
            "cpu_used_milli = {}",
            loaded.cpu_used_milli
        );
    }

    /// CPU cost of sampling at 1 Hz, measured as the CPU time of this
    /// process and its children over 100 samples. The clock is advanced by
    /// one second per sample without sleeping, so the slow readings are
    /// refreshed ten times, exactly as in 100 real seconds.
    ///
    /// The budget is 1 % of one CPU: 10 ms per sample. Recorded results are
    /// in the crate documentation. Run it alone, since other tests in the
    /// same process add to the measured time:
    ///
    /// ```text
    /// cargo test -p bella-probe --lib -- --ignored --nocapture sampling_cost
    /// ```
    #[test]
    #[ignore = "measures process CPU time; run alone"]
    fn sampling_cost_of_100_samples() {
        let dir = tempfile::tempdir().unwrap();
        let mut sampler = Sampler::new(dir.path());
        let start = Instant::now() + Duration::from_secs(1);
        sampler.sample_at(start, ManagedUsage::default());

        let cpu_before = sys::os::process_cpu_time();
        let wall_before = Instant::now();
        for second in 1..=100u64 {
            let usage = ManagedUsage {
                cpu_time_ms_total: second * 100,
                memory_bytes: 1 << 30,
            };
            let sample = sampler.sample_at(start + Duration::from_secs(second), usage);
            std::hint::black_box(sample);
        }
        let cpu = sys::os::process_cpu_time().saturating_sub(cpu_before);
        let wall = wall_before.elapsed();
        let per_sample = cpu / 100;
        println!(
            "sampling cost: {:?} CPU per sample ({:.3} % of one CPU at 1 Hz), {:?} wall for 100 samples",
            per_sample,
            per_sample.as_secs_f64() * 100.0,
            wall
        );
        assert!(
            per_sample < Duration::from_millis(10),
            "sampling costs {per_sample:?} of CPU per sample, over the 1 % budget"
        );
    }
}
