//! Randomized sequences of valid and invalid operations, checked after
//! every step against an independent model of what must be held.

mod common;

use bella_core::Clock;
use bella_core::protocol::{AttemptRef, ExecutionAuthority, LocalAttemptState};
use bella_core::resources::ResourceVector;
use bella_journal::{HoldOutcome, Journal, JournalError, RevokeOutcome, StartDecision};
use common::*;
use std::time::Duration;

/// xorshift64*: deterministic, so a failure names a seed that reproduces it.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

const STATES: [LocalAttemptState; 10] = [
    LocalAttemptState::OfferHeld,
    LocalAttemptState::StartAuthorized,
    LocalAttemptState::Preparing,
    LocalAttemptState::Running,
    LocalAttemptState::EnvironmentHeld,
    LocalAttemptState::Collecting,
    LocalAttemptState::Cleaning,
    LocalAttemptState::Released,
    LocalAttemptState::Quarantined,
    LocalAttemptState::LostAcrossBoot,
];

/// What the test believes about one attempt, updated only from the
/// answers the journal gave.
struct Model {
    attempt: AttemptRef,
    reservation: ResourceVector,
    state: LocalAttemptState,
    expires_at: bella_core::UnixMillis,
    authorized: bool,
    scope: bool,
}

/// The transition table, written out again so the test does not ask the
/// implementation what the implementation should do.
fn table_allows(from: LocalAttemptState, to: LocalAttemptState) -> bool {
    use LocalAttemptState::*;
    match from {
        OfferHeld | Released => false,
        StartAuthorized => matches!(to, Preparing | Cleaning),
        Preparing => matches!(to, Running | Cleaning),
        Running => matches!(to, EnvironmentHeld | Collecting | Cleaning),
        EnvironmentHeld => matches!(to, Collecting | Cleaning),
        Collecting => matches!(to, Cleaning),
        Cleaning => matches!(to, Released | Quarantined),
        Quarantined | LostAcrossBoot => matches!(to, Cleaning),
    }
}

fn random_reservation(rng: &mut Rng) -> ResourceVector {
    let mut reservation = ResourceVector::new(
        500 * (1 + rng.below(16)),
        (1 + rng.below(32)) * GIB / 4,
        rng.below(50) * GIB,
    );
    if rng.below(3) == 0 {
        reservation = reservation.with_token("ios-simulator", 1 + rng.below(2));
    }
    if rng.below(5) == 0 {
        reservation = reservation.with_token("gpu", 1);
    }
    reservation
}

fn check(journal: &Journal, models: &[Model], seed: u64, step: usize) {
    let context = format!("seed {seed}, step {step}");
    let mut expected = ResourceVector::ZERO;
    for model in models {
        if model.state != LocalAttemptState::Released {
            expected = expected.saturating_add(&model.reservation);
        }
    }
    let held = journal.held_resources().unwrap();
    assert_eq!(held, expected, "held resources, {context}");

    // The same figure from the journal's own records: the sum over exactly
    // the attempts that hold resources, none of them released.
    let records = journal.attempts().unwrap();
    assert_eq!(records.len(), models.len(), "{context}");
    let mut from_records = ResourceVector::ZERO;
    for (record, model) in records.iter().zip(models) {
        assert_eq!(record.attempt, model.attempt, "{context}");
        assert_eq!(record.state, model.state, "{:?}, {context}", model.attempt);
        assert_eq!(record.reservation, model.reservation, "{context}");
        assert_eq!(record.scope.is_some(), model.scope, "{context}");
        assert_eq!(record.authority.is_some(), model.authorized, "{context}");
        assert_eq!(
            record.released_at.is_some(),
            model.state == LocalAttemptState::Released,
            "{context}"
        );
        if record.state.holds_resources() {
            assert_ne!(record.state, LocalAttemptState::Released);
            from_records = from_records.saturating_add(&record.reservation);
        }
    }
    assert_eq!(held, from_records, "{context}");
    let active = journal.active_attempts().unwrap();
    assert!(
        active
            .iter()
            .all(|record| record.state != LocalAttemptState::Released),
        "{context}"
    );
    assert_eq!(
        active.len(),
        models
            .iter()
            .filter(|model| model.state != LocalAttemptState::Released)
            .count(),
        "{context}"
    );
}

fn run_sequence(seed: u64) {
    let clock = clock();
    let journal = memory(&clock);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut models: Vec<Model> = Vec::new();
    // Far enough out that only offers expire in this test.
    let permit = ExecutionAuthority {
        valid_until: T0.plus_secs(30 * 24 * 3600),
        hard_deadline: T0.plus_secs(60 * 24 * 3600),
        disconnected_continuation_secs: 3600,
    };

    for step in 0..500 {
        let now = clock.now();
        let pick = rng.below(models.len().max(1) as u64) as usize;
        match rng.below(12) {
            0 | 1 => {
                let number = models.len() as u32;
                let reservation = random_reservation(&mut rng);
                let expires_at = now.plus(Duration::from_millis(1 + rng.below(20_000)));
                let offer = offer_with(number, 1, reservation.clone(), expires_at);
                assert_eq!(journal.hold_offer(&offer).unwrap(), HoldOutcome::Held);
                models.push(Model {
                    attempt: offer.attempt,
                    reservation,
                    state: LocalAttemptState::OfferHeld,
                    expires_at,
                    authorized: false,
                    scope: false,
                });
            }
            2 | 3 if !models.is_empty() => {
                let model = &mut models[pick];
                let result = journal.authorize_start(&model.attempt, permit);
                match (model.state, model.authorized) {
                    (LocalAttemptState::OfferHeld, _) if now < model.expires_at => {
                        assert_eq!(result.unwrap(), StartDecision::Fresh, "seed {seed}");
                        model.state = LocalAttemptState::StartAuthorized;
                        model.authorized = true;
                    }
                    (LocalAttemptState::OfferHeld, _) | (LocalAttemptState::Released, false) => {
                        assert!(
                            matches!(result, Err(JournalError::OfferNotHeld { .. })),
                            "seed {seed}, step {step}: {result:?}"
                        );
                    }
                    _ => assert_eq!(
                        result.unwrap(),
                        StartDecision::AlreadyAuthorized,
                        "seed {seed}"
                    ),
                }
            }
            4 if !models.is_empty() => {
                let model = &mut models[pick];
                let result = journal.revoke(&model.attempt);
                match (model.state, model.authorized) {
                    (LocalAttemptState::OfferHeld, _) => {
                        assert_eq!(result.unwrap(), RevokeOutcome::Revoked, "seed {seed}");
                        model.state = LocalAttemptState::Released;
                    }
                    (LocalAttemptState::Released, false) => {
                        assert_eq!(
                            result.unwrap(),
                            RevokeOutcome::AlreadyReleased,
                            "seed {seed}"
                        );
                    }
                    _ => assert!(
                        matches!(result, Err(JournalError::InvalidState { .. })),
                        "seed {seed}, step {step}: {result:?}"
                    ),
                }
            }
            5 if !models.is_empty() => {
                let model = &mut models[pick];
                let result = journal.set_scope(&model.attempt.attempt_id, &scope("bella-random"));
                let allowed = !matches!(
                    model.state,
                    LocalAttemptState::OfferHeld
                        | LocalAttemptState::Released
                        | LocalAttemptState::LostAcrossBoot
                );
                assert_eq!(result.is_ok(), allowed, "seed {seed}, step {step}");
                if allowed {
                    model.scope = true;
                }
            }
            6..=10 if !models.is_empty() => {
                let model = &mut models[pick];
                let to = STATES[rng.below(STATES.len() as u64) as usize];
                let result = journal.set_state(&model.attempt.attempt_id, to);
                let expected = model.state == to
                    || (table_allows(model.state, to)
                        && (to != LocalAttemptState::Running || model.scope));
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "seed {seed}, step {step}: {:?} -> {to:?} gave {result:?}",
                    model.state
                );
                if expected {
                    model.state = to;
                }
            }
            _ => {
                clock.advance(Duration::from_millis(rng.below(8_000)));
                let now = clock.now();
                let report = journal.expire(now).unwrap();
                assert!(report.stop.is_empty(), "seed {seed}, step {step}");
                let mut expected = Vec::new();
                for model in &mut models {
                    if model.state == LocalAttemptState::OfferHeld && now >= model.expires_at {
                        model.state = LocalAttemptState::Released;
                        expected.push(model.attempt.clone());
                    }
                }
                assert_eq!(report.released_offers, expected, "seed {seed}, step {step}");
            }
        }
        check(&journal, &models, seed, step);
    }

    // The sequence must have exercised both outcomes to mean anything.
    let released = models
        .iter()
        .filter(|model| model.state == LocalAttemptState::Released)
        .count();
    assert!(released > 0, "seed {seed} released nothing");
    assert!(released < models.len(), "seed {seed} released everything");
}

#[test]
fn held_resources_always_equals_the_sum_over_holding_attempts() {
    for seed in 1..=12 {
        run_sequence(seed);
    }
}

#[test]
fn held_resources_stays_exact_across_a_restart_and_a_reboot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let clock = clock();
    let mut rng = Rng(0xB0A7);
    let mut expected = ResourceVector::ZERO;
    {
        let journal = open(&path, &clock);
        journal.recover(&boot(1)).unwrap();
        for number in 0..12 {
            let reservation = random_reservation(&mut rng);
            let offer = offer_with(number, 1, reservation.clone(), T0.plus_secs(15));
            match number % 4 {
                // Held only: dropped by the reboot.
                0 => {
                    journal.hold_offer(&offer).unwrap();
                }
                // Running: lost across the reboot, still held.
                1 => {
                    run(&journal, &offer, authority(T0));
                    expected = expected.saturating_add(&reservation);
                }
                // Released before the restart.
                2 => {
                    let attempt = run(&journal, &offer, authority(T0));
                    journal
                        .set_state(&attempt.attempt_id, LocalAttemptState::Cleaning)
                        .unwrap();
                    journal.release(&attempt, proof()).unwrap();
                }
                // Quarantined: held until proven clean.
                _ => {
                    let attempt = run(&journal, &offer, authority(T0));
                    journal
                        .set_state(&attempt.attempt_id, LocalAttemptState::Cleaning)
                        .unwrap();
                    journal
                        .set_state(&attempt.attempt_id, LocalAttemptState::Quarantined)
                        .unwrap();
                    expected = expected.saturating_add(&reservation);
                }
            }
        }
    }
    let journal = open(&path, &clock);
    // Before recovery the unstarted offers still count in full.
    let before = journal.held_resources().unwrap();
    assert!(expected.fits_within(&before) && before != expected);
    journal.recover(&boot(2)).unwrap();
    assert_eq!(journal.held_resources().unwrap(), expected);
    assert!(
        journal
            .active_attempts()
            .unwrap()
            .iter()
            .all(|record| record.state == LocalAttemptState::LostAcrossBoot)
    );
}
