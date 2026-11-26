# Cloud capacity

Builderella v0.1 cannot rent a machine. The `bella-cloud` crate contains the
bookkeeping a fleet needs before it may spend money on temporary cloud
workers: the durable record of each request, the budget ledger, the
reconciliation logic and the overflow policy. All of it runs against a
simulated provider. No AWS or GCP code exists, the crate makes no network
calls, and nothing in it can create a billable resource.

This page says what exists, what does not, which properties the code is
tested for, and what an administrator will have to supply before paid
capacity can be turned on.

## What exists in v0.1

| Part | Module | What it does |
| --- | --- | --- |
| Provider contract | `provider` | The blocking `Provider` trait: capabilities, quote, create, find by intention, list owned, delete, janitor status. |
| Simulated provider | `sim` | `SimProvider`: in memory, driven by an injected clock, with a switch for each failure listed under [Faults the simulator can inject](#faults-the-simulator-can-inject). |
| Intentions | `intent` | One row per capacity request, a checked state machine, the provider resources it owns, and an event row for every change. |
| Money ledger | `money` | Allowances, the atomic budget hold, the accrued estimate and the provider-reported actual, in integer micro-units of the billing currency. |
| Engine | `engine` | Request, authorize, provision, reconcile, worker registration, cancel, drain, terminate, sweep and exposure. |
| Overflow policy | `overflow` | A pure function that proposes new workers from the queue, and one that proposes releases. |

The tables live in the controller's SQLite database. `bella_cloud::migrate`
creates them and tracks its own version in `cloud_schema`. The engine takes
its transactions through the `Database` trait, which is implemented for a
plain connection; the controller implements it over the ledger's
`with_transaction`.

## What does not exist

* **Real provider adapters.** Nothing implements `Provider` for AWS, GCP or
  any other cloud. The trait has only been exercised by the simulator, so it
  may need changes when the first real adapter is written.
* **Worker images.** An intention records an image name and passes it to the
  provider. No image is built, published or verified.
* **The provider-side janitor.** The engine refuses to act unless a janitor
  reports recent success, but no janitor is shipped. The simulator's
  `tick()` stands in for one.
* **Billing import.** `record_actual` stores a provider-reported cost when
  something supplies it. Nothing reads a billing export.
* **Controller wiring.** No REST endpoint, CLI command or dashboard view
  calls this crate yet, and the dispatch loop does not run `sweep` or the
  overflow policy.
* **Price history, currency conversion and committed-use discounts.** A
  quote in a currency other than the allowance's is denied, not converted.

## Life of a request

```
Requested → Authorized → Provisioning → Bootstrapping → Ready ⇄ Busy → Draining
                              │                                          │
                              └→ Unknown                  Terminating ←──┘
                                                               │
                                                          Terminated
```

`Failed` and `Terminated` are final. `Quarantined` means the provider's
state contradicts the record and a person has to decide. A transition the
table does not allow is the typed error `CloudError::IllegalTransition`.

1. **`request_capacity`** records the intention. It holds no money and
   calls nothing that creates a resource. It is refused, writing nothing,
   for any reason listed under [Gates](#gates).
2. **`authorize`** takes a quote and, in one transaction, checks every
   allowance rule, inserts the hold and moves the intention to
   `Authorized`.
3. **`provision`** writes `Provisioning` and then calls the provider's
   `create` with the intention's identifier as the idempotency token.
4. **`on_worker_registered`** admits the new worker once, when it presents
   the bootstrap string of a `Bootstrapping` intention that is neither
   cancelled nor expired. Only the SHA-256 of the string is stored.
5. **`terminate`** deletes every resource and closes the intention when the
   provider reports that nothing is left.
6. **`sweep`** runs periodically and finishes whatever was interrupted.

Every intention has an absolute expiry: request time plus a startup
allowance (10 minutes by default) plus the requested lifetime. Resources are
created with that expiry, and nothing extends it.

## Money

Amounts are integers in micro-units (millionths) of the provider's billing
currency. Three numbers are stored separately and can each be queried with
`money(intention)`:

| Number | Meaning | Set by |
| --- | --- | --- |
| Admission hold | The most the intention may cost, reserved against the allowance before the provider is asked for anything. | `authorize` |
| Accrued estimate | Tracked resource time at the hourly rates the provider reported, with the time of the estimate and the assumptions behind it. Frozen at settlement. | Computed |
| Provider-reported actual | What the provider says it billed, provisional or final. | `record_actual` |

The hold is

```
startup + permitted lifetime × hourly price + billing minimum + allowed retries
```

where startup is the hourly price for the startup allowance, the billing
minimum is the provider's minimum commitment in full, and each further
permitted create attempt adds one startup and one minimum. Every term
rounds up. The minimum is added rather than netted against the run because
a quote does not say which part of the hourly price the commitment covers.
The hold is therefore an upper bound, and can exceed the real cost by up to
one run.

An allowance is set per scope (the fleet, or one project) and period (UTC
day or UTC calendar month):

| Field | Limit |
| --- | --- |
| `limit_micros` | Outstanding holds plus spend settled in the current period. |
| `max_hourly_micros` | Combined quoted hourly rate of intentions holding budget. |
| `max_active` | Intentions holding budget. |
| `max_pending` | Intentions authorized but not yet usable. |
| `allowed_presets`, `allowed_regions`, `allowed_purchase` | Allow-lists. An empty list allows nothing. |
| `max_lifetime_secs` | Requested lifetime. |
| `max_attempts` | Create calls for one request, across its retries. |

Authorization checks every allowance that applies: the fleet's and the
project's. Both must exist. Rules worth knowing:

* An outstanding hold counts in every period until it is released. A day or
  month rolling over does not remove it.
* An outstanding hold counts as the larger of the hold and the accrued
  estimate, so a resource that outlives its hold keeps consuming the
  allowance.
* Settled spend counts in the period in which the hold was released. It is
  the final provider figure when there is one, otherwise the larger of the
  estimate and any provisional figure.
* A quote older than five minutes, or for a different preset, region,
  purchase model or lifetime than the intention, is denied.
* A hold is released only when the intention is `Terminated` or `Failed`
  and the provider reports no resource for it.

## Gates

Paid provisioning cannot start by accident, because each of these has to be
changed deliberately and each defaults to "no":

1. **No provider.** The only `Provider` implementation is the simulator,
   which reports `Billing::Simulated`.
2. **Engine switch.** A provider that reports `Billing::Real` is refused
   unless `EngineConfig::allow_paid_providers` is set. It is `false` by
   default and nothing in the repository sets it.
3. **Project mode.** `ProjectPolicy::cloud` defaults to `ExistingOnly`,
   which refuses every request. `Manual` refuses automatic requests,
   `AutoOverflow` refuses manual ones, `Hybrid` allows both.
4. **Allowances.** None exist until an administrator sets a fleet
   allowance and a project allowance. `set_allowance` rejects any other
   role. An allowance with empty allow-lists still permits nothing.
5. **Janitor.** Requests and create calls are refused while the
   provider-side janitor is missing, has never succeeded, last succeeded
   more than 30 minutes ago, or cannot be asked.
6. **Overflow policy.** `OverflowPolicy::default()` is disabled, has no
   presets and keeps no warm pool.

Also refused: LAN-only source (no priority or escalation flag changes
this), a manual request from a viewer, a preset, region or purchase model
the provider does not offer, and a provider without provider-side expiry.

## Safety properties

Each property has a test against the simulator with the matching fault.
The acceptance tests are in `crates/bella-cloud/tests/acceptance.rs`.

| Property | Test |
| --- | --- |
| No capacity without a healthy janitor; the refusal says how to repair it (A48). | `a48_no_capacity_without_a_healthy_janitor` |
| A lost create response yields one instance and one hold. No second create is issued until a listing has settled the question, and an empty listing is not trusted before the settling period (A49). | `a49_*` |
| If the controller dies after creating an instance, provider-side expiry removes it and the next sweep settles the hold (A50, modelled). | `a50_*` |
| A worker whose intention was cancelled, expired or is unknown is denied, and its resources are scheduled for deletion (A51). | `a51_*` |
| Two authorizations racing for the last of an allowance: one succeeds (A52). | `a52_racing_authorizations_take_the_last_of_an_allowance_once` |
| A vanished spot instance fails the intention as an infrastructure interruption. The partial result is not promoted. A retry is permitted only by attempts, time and budget (A53). | `a53_*` |
| Unavailable spot capacity never becomes on-demand (A54). | `a54_*` |
| LAN-only source never gets cloud capacity (A55). | `a55_lan_only_source_is_refused_whatever_the_priority` |
| A disk and address left behind by a deletion keep the intention open, visible and priced, with its hold, until they are deleted (A56). | `a56_*` |
| A host under a 24-hour minimum keeps its hold and stays tracked after the job ends (A58). | `a58_a_minimum_commitment_keeps_the_hold_and_the_host_until_release` |

How the uncertain cases are decided:

* **Create timed out.** The intention becomes `Unknown` and keeps its hold.
  `reconcile` lists the resources carrying the token. One instance is
  adopted. None, after a two-minute settling period, lets `provision` retry
  with the same token until the attempts are used up. More than one
  quarantines the intention.
* **Unavailable spot.** If the allowance permits only spot, the intention
  returns to `Authorized` and waits. If it also permits on-demand, the
  intention fails and reports that an on-demand request would be allowed.
  The engine never makes that request. A retry of a failed intention must
  keep its purchase model.
* **Delete timed out.** The next listing decides. A resource is treated as
  gone when a delete succeeded, or when it is missing from a listing and
  either an earlier listing showed it or the settling period has passed
  since it was first recorded.
* **Create outcome never observed.** If the question can only be checked
  after the expiry, the estimate charges the quoted rate from the create
  call to the expiry and says so in its assumptions.
* **Orphans.** A provider resource that no open intention accounts for is
  reported and recorded. It is deleted only if it carries this fleet's
  label. Otherwise it stays in `exposure()` until the provider stops
  listing it.

`exposure()` lists every unsettled intention that holds budget or owns a
resource, with its hold, accrued estimate, current hourly rate and
resources, plus every open orphan.

## Overflow policy

`evaluate(policy, demand, capacity, history, now)` proposes workers. It is
pure: no clock, no database, the same answer for the same inputs. A
proposal still has to pass `request_capacity` and `authorize`.

A queued job counts only if its cloud mode is `AutoOverflow` or `Hybrid`,
its source is not LAN-only, and it fires a trigger: predicted wait at or
above a threshold, time in the queue at or above a threshold, a deadline
closer than the predicted wait plus a margin, or an envelope that fits no
enrolled machine of its platform.

Limits on what it proposes:

* Capacity that is requested, provisioning or bootstrapping counts. Jobs it
  was requested for are not asked for again and its free room is filled
  first, so twenty jobs that fit one worker produce one proposal.
* At most `max_pending` pending workers, `max_active` workers in total and
  `max_launches` launches per `launch_window_ms`.
* A preset whose worker failed to bootstrap is skipped for
  `bootstrap_failure_cooldown_ms`.
* Hysteresis: `release_candidates` releases a worker only after
  `idle_release_after_ms` idle and only when no job it could run is
  predicted to wait `release_wait_ms`, which is lower than the request
  threshold. After a release, the wait and queue-age triggers stay quiet on
  that platform for `reacquire_cooldown_ms`. Deadline and size triggers are
  not delayed.
* A worker is never shared between projects, because budgets are per
  project.

A property test runs 300 random demand streams and checks that the limits
hold and that evaluating again after recording the proposals asks for
nothing.

## Faults the simulator can inject

Create succeeds but the response is lost; create fails with a chosen error;
quota exceeded; spot capacity unavailable; delayed readiness; a spot
instance withdrawn without notice; delete succeeds but the response is
lost; a delete that leaves the disk and address behind; resources that
cannot be deleted; a provider that is unreachable; a listing that lags
behind creation; an ignored idempotency token; a price change; a janitor
that is missing, stale or stalled; and provider-side expiry, which removes
expired resources when `tick()` is called.

## Before paid capacity can be enabled

None of this can be supplied through configuration today, because the
adapter does not exist. When it does, an administrator will have to
provide:

1. **A provider adapter** implementing `Provider`, reviewed against the
   requirements in the trait's documentation: idempotent create by token,
   idempotent delete, listings that include secondary resources, and
   definite errors only when nothing was changed.
2. **A provider account** dedicated to the fleet, with credentials limited
   to creating, labelling, listing and deleting the resource kinds the
   presets use. The credentials never go into the repository or this
   database.
3. **Provider-side quotas and a billing alarm**, set in the provider
   account, as a limit that holds even if this ledger is wrong.
4. **A provider-side janitor** (see below), installed and reporting
   success.
5. **Worker images** per preset that start the worker and enroll it with
   the one-use bootstrap string.
6. **Presets, regions and prices**: which machine shapes may be used, and a
   quote source the adapter can stand behind.
7. **Allowances**: a fleet allowance and one per project, each with a
   currency, a limit per period, rate and count limits, allow-lists, a
   maximum lifetime and a maximum number of attempts.
8. **Project cloud modes** other than `ExistingOnly` for the projects that
   may use the cloud, and confirmation that none of them is LAN-only.
9. **The engine switch** `allow_paid_providers`.
10. **A reconciliation routine**: who reads `exposure()` and the sweep
    report, and who feeds provider invoices into `record_actual`.

### Provider-side janitor

The janitor is a job that runs inside the provider account, independently
of the controller. On a schedule it deletes every resource labelled with
the fleet (`builderella-fleet`) whose expiry has passed, and records when
it last completed. It exists so that a controller that is lost, offline or
wrong cannot leave machines running indefinitely.

The engine only reads its status through `Provider::janitor_status`. It
refuses to request or create capacity when the janitor is not installed,
has never succeeded, last succeeded more than
`EngineConfig::janitor_max_staleness_secs` ago (30 minutes by default), or
cannot be asked. To repair a refusal: check that the job is deployed and
scheduled, that its credentials can list and delete the fleet's resources,
and that its last run succeeded; then retry.

v0.1 ships no janitor. `SimProvider::tick()` models one.

## Known limits

* The contract has one implementation, the simulator. Real providers have
  eventual consistency, partial failures and rate limits that the simulator
  only approximates.
* The estimate is not a bill. It uses the hourly rates the provider
  reported for each resource and ignores taxes, data transfer, per-second
  rounding rules and discounts.
* A resource that disappears between two sweeps is charged up to the sweep
  that noticed, or up to its expiry when that has passed.
* The periods are UTC. A provider that bills in another time zone will not
  line up at the edges.
* There is one controller and one writer. The race test uses several
  connections to one database file; it does not cover several controllers.
* The crate was built and tested on Windows. It has no platform-specific
  code, but it has not been run on Linux or macOS yet.
