//! Random interleavings of offer, reply, start, events, release, cancel,
//! expiry and uncertainty against a small model. After every step the
//! ledger's held capacity must equal the sum of the reservations the model
//! still considers unreleased, and never exceed the machine's capacity.

use crate::common::{Fixture, exited, fixture, lost, proof, spec};
use bella_core::UnixMillis;
use bella_core::ids::{JobId, MachineId};
use bella_core::job::{JobPhase, Outcome, QueueReasonCode};
use bella_core::protocol::{AttemptEvent, AttemptRef, OfferDecision, ReleaseProof};
use bella_core::resources::ResourceVector;
use bella_ledger::{
    CancelOutcome, EventDisposition, LedgerError, OfferReplyOutcome, ReleaseOutcome, RevokeOutcome,
};

/// xorshift64*: small, seeded and good enough to pick operations.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone, Debug)]
struct Live {
    attempt: AttemptRef,
    machine: usize,
    reserved: ResourceVector,
    expires_at: UnixMillis,
    accepted: bool,
    started: bool,
    exited: bool,
    next_seq: u64,
}

#[derive(Clone, Debug)]
enum State {
    Queued,
    /// Offered or running: the reservation is held.
    Live(Live),
    Done,
}

struct Model {
    f: Fixture,
    machines: Vec<MachineId>,
    capacity: ResourceVector,
    max_concurrent: u32,
    jobs: Vec<(JobId, State)>,
    /// Attempts whose reservation was released, for replaying old messages.
    released: Vec<AttemptRef>,
}

fn rejection() -> OfferDecision {
    OfferDecision::Rejected {
        code: QueueReasonCode::MemoryPressure,
        message: "no".into(),
    }
}

impl Model {
    fn new() -> Self {
        let f = fixture();
        let machines = vec![f.machine("alpha"), f.machine("beta"), f.machine("gamma")];
        let mut model = Self {
            f,
            machines,
            capacity: ResourceVector::new(4000, 8 << 30, 0).with_token("sim", 2),
            max_concurrent: 3,
            jobs: Vec::new(),
            released: Vec::new(),
        };
        for machine in &model.machines {
            let mut policy = model.f.ledger.machine_policy(machine).unwrap();
            policy.tokens.insert("sim".into(), 2);
            model
                .f
                .ledger
                .replace_machine_policy(&model.f.admin, machine, &policy)
                .unwrap();
        }
        for _ in 0..10 {
            let job = model.new_job();
            model.jobs.push((job, State::Queued));
        }
        model
    }

    fn new_job(&self) -> JobId {
        let mut spec = spec();
        // Plenty of attempts, so retries keep the jobs moving.
        spec.max_attempts = 1000;
        self.f.submit_spec("app", spec)
    }

    fn lives_on(&self, machine: usize) -> impl Iterator<Item = &Live> {
        self.jobs.iter().filter_map(move |(_, state)| match state {
            State::Live(live) if live.machine == machine => Some(live),
            _ => None,
        })
    }

    fn expected_held(&self, machine: usize) -> ResourceVector {
        self.lives_on(machine)
            .fold(ResourceVector::ZERO, |sum, live| {
                sum.checked_add(&live.reserved).unwrap()
            })
    }

    /// The invariant, checked after every step.
    fn check(&self, step: usize, op: &str) {
        for (index, machine) in self.machines.iter().enumerate() {
            let view = self.f.ledger.machine_view(machine).unwrap();
            let expected = self.expected_held(index);
            assert_eq!(
                view.reserved, expected,
                "step {step} ({op}): held differs from the unreleased reservations"
            );
            assert!(
                view.reserved.fits_within(&self.capacity),
                "step {step} ({op}): machine {index} is over capacity: {:?}",
                view.reserved
            );
            let held = self.f.ledger.machine_attempts(machine).unwrap();
            assert_eq!(
                held.len(),
                self.lives_on(index).count(),
                "step {step} ({op})"
            );
            assert!(
                held.len() <= self.max_concurrent as usize,
                "step {step} ({op})"
            );
        }
    }

    fn requeue_or_finish(&mut self, index: usize, state: State) {
        if let State::Live(live) = &self.jobs[index].1 {
            self.released.push(live.attempt.clone());
        }
        self.jobs[index].1 = state;
    }

    fn step(&mut self, rng: &mut Rng) -> &'static str {
        // Mostly follow the jobs that hold a reservation, so that attempts
        // get through their whole life before their offers expire.
        let live: Vec<usize> = (0..self.jobs.len())
            .filter(|index| matches!(self.jobs[*index].1, State::Live(_)))
            .collect();
        let index = if !live.is_empty() && rng.below(4) != 0 {
            live[rng.below(live.len())]
        } else {
            rng.below(self.jobs.len())
        };
        let job = self.jobs[index].0.clone();
        let state = self.jobs[index].1.clone();
        let epoch = self.f.ledger.epoch();
        // Operations are drawn from what is interesting for the job's
        // state, including ones that must be refused in that state.
        const OFFER: usize = 0;
        let menu: &[usize] = match (rng.below(20), &state) {
            (0, _) => &[10],
            (1, _) => &[11],
            (_, State::Queued) => &[OFFER, OFFER, OFFER, OFFER, OFFER, OFFER, OFFER, OFFER, 9],
            (_, State::Live(live)) if !live.started => &[3, 3, 3, 3, 6, 6, 6, 6, 4, 5, 9, OFFER],
            (_, State::Live(live)) if !live.exited => &[7, 7, 7, 7, 8, 8, 4, 5, 6, 9, OFFER],
            (_, State::Live(_)) => &[8, 8, 8, 8, 8, 4, 5, 6, 9, OFFER],
            (_, State::Done) => &[12, 12, 12, 12, 9, OFFER],
        };
        match (menu[rng.below(menu.len())], state) {
            (OFFER, state) => {
                let machine = rng.below(self.machines.len());
                let reserved = ResourceVector::new(
                    500 * (1 + rng.below(4)) as u64,
                    (1 + rng.below(4) as u64) << 30,
                    0,
                )
                .with_token("sim", rng.below(3) as u64);
                let expires_at = self.f.now().plus_secs(15);
                let result = self.f.ledger.create_offer(
                    &job,
                    &self.machines[machine],
                    &reserved,
                    &self.capacity,
                    Some(self.max_concurrent),
                    expires_at,
                );
                let fits = self
                    .expected_held(machine)
                    .checked_add(&reserved)
                    .unwrap()
                    .fits_within(&self.capacity);
                let slots = self.lives_on(machine).count() < self.max_concurrent as usize;
                match (matches!(state, State::Queued), fits, slots) {
                    (true, true, true) => {
                        let offer = result.unwrap();
                        self.jobs[index].1 = State::Live(Live {
                            attempt: offer.attempt,
                            machine,
                            reserved,
                            expires_at,
                            accepted: false,
                            started: false,
                            exited: false,
                            next_seq: 1,
                        });
                    }
                    (false, _, _) => {
                        assert!(matches!(result, Err(LedgerError::InvalidState(_))));
                    }
                    (true, false, _) => {
                        assert!(matches!(result, Err(LedgerError::Capacity { .. })));
                    }
                    (true, true, false) => {
                        assert!(matches!(result, Err(LedgerError::ConcurrencyLimit { .. })));
                    }
                }
                "offer"
            }
            (3, State::Live(mut live)) if !live.started => {
                self.f
                    .ledger
                    .record_offer_reply(&live.attempt, &OfferDecision::Accepted)
                    .unwrap();
                live.accepted = true;
                self.jobs[index].1 = State::Live(live);
                "accept"
            }
            (4, State::Live(live)) => {
                let result = self
                    .f
                    .ledger
                    .record_offer_reply(&live.attempt, &rejection());
                if live.started {
                    assert!(matches!(result, Err(LedgerError::InvalidState(_))));
                } else {
                    assert_eq!(result.unwrap(), OfferReplyOutcome::Requeued);
                    self.requeue_or_finish(index, State::Queued);
                }
                "reject"
            }
            (5, State::Live(live)) => {
                let result = self.f.ledger.revoke_offer(&live.attempt, "test");
                if live.started {
                    assert!(matches!(result, Err(LedgerError::InvalidState(_))));
                } else {
                    assert_eq!(result.unwrap(), RevokeOutcome::Revoked);
                    self.requeue_or_finish(index, State::Queued);
                }
                "revoke"
            }
            (6, State::Live(mut live)) => {
                let result = self.f.ledger.commit_start(&live.attempt, epoch);
                let fresh = self.f.now() < live.expires_at;
                if live.started {
                    assert!(result.unwrap().already_committed);
                } else if live.accepted && fresh {
                    assert!(!result.unwrap().already_committed);
                    live.started = true;
                    self.jobs[index].1 = State::Live(live);
                } else {
                    assert!(matches!(result, Err(LedgerError::InvalidState(_))));
                }
                "start"
            }
            (7, State::Live(mut live)) if live.started && !live.exited => {
                let outcome = match rng.below(3) {
                    0 => Outcome::Succeeded,
                    1 => lost(),
                    _ => Outcome::Failed {
                        step: None,
                        exit_code: Some(1),
                    },
                };
                let disposition = self.f.event(&live.attempt, live.next_seq, exited(outcome));
                assert_eq!(disposition.unwrap(), EventDisposition::Applied);
                live.next_seq += 1;
                live.exited = true;
                self.jobs[index].1 = State::Live(live);
                "exit"
            }
            (8, State::Live(live)) if live.started => {
                // Sometimes the proof is not good enough; then nothing is freed.
                if rng.below(4) == 0 {
                    let unverified = ReleaseProof {
                        scope_empty: false,
                        workspace_removed: true,
                        method: "timeout".into(),
                    };
                    let outcome = self.f.ledger.record_release(&live.attempt, &unverified);
                    assert_eq!(outcome.unwrap(), ReleaseOutcome::Quarantined);
                    return "quarantine";
                }
                let outcome = self
                    .f
                    .ledger
                    .record_release(&live.attempt, &proof())
                    .unwrap();
                assert!(matches!(outcome, ReleaseOutcome::Released(_)));
                let phase = self.f.ledger.job(&job).unwrap().phase;
                let next = match phase {
                    JobPhase::Queued => State::Queued,
                    JobPhase::Finished => State::Done,
                    other => panic!("released job is {other:?}"),
                };
                self.requeue_or_finish(index, next);
                "release"
            }
            (9, state) => {
                let outcome = self.f.ledger.request_cancel(&job).unwrap();
                match state {
                    State::Done => assert_eq!(outcome, CancelOutcome::AlreadyFinished),
                    State::Queued => {
                        assert_eq!(outcome, CancelOutcome::Cancelled { revoked: None });
                        self.jobs[index].1 = State::Done;
                    }
                    State::Live(live) if live.started => {
                        assert!(matches!(outcome, CancelOutcome::Requested { .. }));
                    }
                    State::Live(_) => {
                        assert!(matches!(
                            outcome,
                            CancelOutcome::Cancelled { revoked: Some(_) }
                        ));
                        self.requeue_or_finish(index, State::Done);
                    }
                }
                "cancel"
            }
            (10, _) => {
                // Time passes and unanswered offers expire.
                self.f.advance_secs(rng.below(7) as u64);
                let now = self.f.now();
                let expired = self.f.ledger.expire_offers(now).unwrap();
                let mut expected = 0;
                for index in 0..self.jobs.len() {
                    let due = matches!(
                        &self.jobs[index].1,
                        State::Live(live) if !live.started && live.expires_at <= now
                    );
                    if due {
                        expected += 1;
                        self.requeue_or_finish(index, State::Queued);
                    }
                }
                assert_eq!(expired.len(), expected);
                "expire"
            }
            (11, _) => {
                // Losing contact, or hearing old messages again, frees nothing.
                let machine = rng.below(self.machines.len());
                self.f
                    .ledger
                    .mark_machine_uncertain(&self.machines[machine])
                    .unwrap();
                if !self.released.is_empty() {
                    let old = self.released[rng.below(self.released.len())].clone();
                    let again = self.f.ledger.record_release(&old, &proof()).unwrap();
                    assert_eq!(again, ReleaseOutcome::AlreadyReleased);
                    let replay = self.f.event(&old, 1, AttemptEvent::Preparing);
                    assert!(
                        matches!(
                            replay,
                            Ok(EventDisposition::Duplicate | EventDisposition::AttemptReleased)
                                | Err(LedgerError::StaleGeneration { .. })
                        ),
                        "{replay:?}"
                    );
                }
                "uncertain"
            }
            (12, State::Done) => {
                // A finished job's place is taken by a new submission.
                self.jobs[index] = (self.new_job(), State::Queued);
                "submit"
            }
            _ => "skip",
        }
    }
}

#[test]
fn capacity_matches_the_unreleased_reservations_under_random_interleavings() {
    for seed in 1..=6u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut model = Model::new();
        let mut applied = std::collections::BTreeMap::new();
        for step in 0..800 {
            let op = model.step(&mut rng);
            *applied.entry(op).or_insert(0u32) += 1;
            model.check(step, op);
        }
        // The run must actually have exercised the interesting transitions.
        for op in [
            "offer",
            "accept",
            "reject",
            "revoke",
            "start",
            "exit",
            "release",
            "cancel",
            "expire",
            "quarantine",
            "uncertain",
            "submit",
        ] {
            assert!(
                applied.get(op).copied().unwrap_or(0) > 0,
                "seed {seed}: no `{op}` happened"
            );
        }
        // Nothing is left behind once every live attempt is released.
        for index in 0..model.jobs.len() {
            if let State::Live(live) = model.jobs[index].1.clone() {
                if live.started {
                    model
                        .f
                        .ledger
                        .record_release(&live.attempt, &proof())
                        .unwrap();
                } else {
                    model.f.ledger.revoke_offer(&live.attempt, "end").unwrap();
                }
                model.requeue_or_finish(index, State::Done);
            }
        }
        model.check(800, "drain");
        for machine in &model.machines {
            assert!(model.f.held(machine).is_zero(), "seed {seed}");
        }
    }
}
