//! An in-memory provider for tests and for running the engine without a
//! cloud account.
//!
//! `SimProvider` is deterministic: identifiers come from a counter, time
//! comes from the injected clock, and nothing happens in the background.
//! Provider-side expiry runs only when [`SimProvider::tick`] is called, which
//! models a janitor that works independently of the controller. Every
//! failure the engine has to survive is a switch in [`SimFaults`].

use crate::convert::prorate_up;
use crate::provider::{
    Billing, Capabilities, CreateAck, CreateRequest, JanitorStatus, LABEL_FLEET, LABEL_INTENT,
    OwnedResource, Preset, Provider, ProviderError, PurchaseModel, Quote, ResourceKind,
    ResourceState,
};
use bella_core::defaults::GIB;
use bella_core::platform::{Arch, Os, Platform};
use bella_core::resources::ResourceVector;
use bella_core::{Clock, CloudIntentId, FleetId, UnixMillis};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Fault switches. All are off by default.
#[derive(Clone, Debug, Default)]
pub struct SimFaults {
    /// The next this-many creates take effect and then report `Timeout`.
    pub lose_create_response: u32,
    /// Each queued error fails one create without creating anything.
    pub fail_create: VecDeque<ProviderError>,
    /// Creates fail with `QuotaExceeded`.
    pub quota_exceeded: bool,
    /// Spot creates fail with `CapacityUnavailable`; on-demand still works.
    pub spot_capacity_unavailable: bool,
    /// A new instance stays `Pending` for this long.
    pub readiness_delay_secs: u64,
    /// The next this-many deletes take effect and then report `Timeout`.
    pub lose_delete_response: u32,
    /// Removing an instance leaves its disk and address behind.
    pub leak_secondary_on_delete: bool,
    /// Deletes of these kinds fail with `Unavailable` and delete nothing.
    pub undeletable: BTreeSet<ResourceKind>,
    /// Every call fails with `Unavailable` and changes nothing.
    pub unavailable: bool,
    /// A new resource is missing from listings for this long.
    pub listing_lag_secs: u64,
    /// A repeated create with a known token creates a second set of
    /// resources, as a provider without idempotency tokens would.
    pub ignore_idempotency_token: bool,
    /// The janitor is installed but does nothing when ticked.
    pub janitor_stalled: bool,
    /// `list_owned` also returns resources without the fleet label, as an
    /// adapter that lists by a looser criterion would.
    pub list_unlabelled: bool,
}

/// One `create` call as the provider saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimCreateCall {
    pub intent: CloudIntentId,
    pub purchase: PurchaseModel,
    pub at: UnixMillis,
    /// Whether this call created new resources.
    pub created: bool,
}

#[derive(Clone, Debug)]
struct SimPreset {
    preset: Preset,
    on_demand_hourly: u64,
    spot_hourly: Option<u64>,
    dedicated_host: bool,
}

impl SimPreset {
    fn price(&self, purchase: PurchaseModel) -> Option<u64> {
        match purchase {
            PurchaseModel::OnDemand => Some(self.on_demand_hourly),
            PurchaseModel::Spot => self.spot_hourly,
        }
    }
}

#[derive(Clone, Debug)]
struct SimResource {
    resource: OwnedResource,
    purchase: PurchaseModel,
    ready_at: UnixMillis,
    /// Resources created by one create call share a group.
    group: String,
}

struct Inner {
    name: String,
    billing: Billing,
    currency: String,
    regions: Vec<String>,
    presets: Vec<SimPreset>,
    disk_hourly: u64,
    address_hourly: u64,
    faults: SimFaults,
    resources: BTreeMap<String, SimResource>,
    tokens: BTreeSet<String>,
    next_serial: u64,
    janitor_installed: bool,
    janitor_last_success: Option<UnixMillis>,
    create_calls: Vec<SimCreateCall>,
    delete_calls: Vec<String>,
}

/// The simulated provider. See the module documentation.
pub struct SimProvider {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

impl SimProvider {
    /// Hourly price of the boot disk every simulated instance gets.
    pub const DISK_HOURLY_MICROS: u64 = 4_000;
    /// Hourly price of the address every simulated instance gets.
    pub const ADDRESS_HOURLY_MICROS: u64 = 5_000;
    /// Minimum commitment of the dedicated-host preset.
    pub const HOST_COMMITMENT_SECS: u64 = 24 * 60 * 60;

    /// A provider named `sim` with a small fixed catalogue, simulated
    /// billing, and a janitor that is installed and has just succeeded.
    ///
    /// Presets: `linux-small`, `linux-large` and `windows-large` are sold
    /// on demand and as spot; `mac-dedicated` is a dedicated host with a
    /// 24-hour minimum commitment, sold on demand only. Regions: `sim-east`
    /// and `sim-west`. Currency `USD`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        let both = vec![PurchaseModel::OnDemand, PurchaseModel::Spot];
        let preset = |name: &str,
                      os: Os,
                      arch: Arch,
                      cpus: u64,
                      memory_gib: u64,
                      on_demand: u64,
                      spot: Option<u64>| SimPreset {
            preset: Preset {
                name: name.to_string(),
                platform: Platform { os, arch },
                resources: ResourceVector::new(cpus * 1000, memory_gib * GIB, 100 * GIB),
                purchase: if spot.is_some() {
                    both.clone()
                } else {
                    vec![PurchaseModel::OnDemand]
                },
                minimum_commitment_secs: 0,
            },
            on_demand_hourly: on_demand,
            spot_hourly: spot,
            dedicated_host: false,
        };
        let mut mac = preset(
            "mac-dedicated",
            Os::Macos,
            Arch::Aarch64,
            8,
            16,
            1_200_000,
            None,
        );
        mac.preset.minimum_commitment_secs = Self::HOST_COMMITMENT_SECS;
        mac.dedicated_host = true;
        let presets = vec![
            preset(
                "linux-small",
                Os::Linux,
                Arch::X86_64,
                4,
                8,
                200_000,
                Some(60_000),
            ),
            preset(
                "linux-large",
                Os::Linux,
                Arch::X86_64,
                16,
                64,
                800_000,
                Some(240_000),
            ),
            preset(
                "windows-large",
                Os::Windows,
                Arch::X86_64,
                16,
                64,
                1_500_000,
                Some(500_000),
            ),
            mac,
        ];
        Self {
            clock,
            inner: Mutex::new(Inner {
                name: "sim".to_string(),
                billing: Billing::Simulated,
                currency: "USD".to_string(),
                regions: vec!["sim-east".to_string(), "sim-west".to_string()],
                presets,
                disk_hourly: Self::DISK_HOURLY_MICROS,
                address_hourly: Self::ADDRESS_HOURLY_MICROS,
                faults: SimFaults::default(),
                resources: BTreeMap::new(),
                tokens: BTreeSet::new(),
                next_serial: 1,
                janitor_installed: true,
                janitor_last_success: Some(now),
                create_calls: Vec::new(),
                delete_calls: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the fault switches.
    pub fn faults(&self, change: impl FnOnce(&mut SimFaults)) {
        change(&mut self.lock().faults);
    }

    /// Makes the provider claim real or simulated billing. It never bills
    /// anything either way; this exists to test the engine's paid-provider gate.
    pub fn set_billing(&self, billing: Billing) {
        self.lock().billing = billing;
    }

    /// Changes the hourly price of a preset. Returns false when the preset
    /// does not exist or is not sold under that purchase model.
    pub fn set_price(&self, preset: &str, purchase: PurchaseModel, hourly_micros: u64) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.presets.iter_mut().find(|p| p.preset.name == preset) else {
            return false;
        };
        match purchase {
            PurchaseModel::OnDemand => {
                entry.on_demand_hourly = hourly_micros;
                true
            }
            PurchaseModel::Spot => match entry.spot_hourly.as_mut() {
                Some(price) => {
                    *price = hourly_micros;
                    true
                }
                None => false,
            },
        }
    }

    /// Sets what `janitor_status` reports.
    pub fn set_janitor(&self, installed: bool, last_success_at: Option<UnixMillis>) {
        let mut inner = self.lock();
        inner.janitor_installed = installed;
        inner.janitor_last_success = last_success_at;
    }

    /// One run of the provider-side janitor. When it is installed and not
    /// stalled it removes every resource past its expiry, except a host that
    /// is still under its minimum commitment, and records a success. Returns
    /// the identifiers it removed.
    pub fn tick(&self) -> Vec<String> {
        let now = self.clock.now();
        let mut inner = self.lock();
        if !inner.janitor_installed || inner.faults.janitor_stalled {
            return Vec::new();
        }
        let expired: Vec<String> = inner
            .resources
            .values()
            .filter(|entry| {
                let resource = &entry.resource;
                let past_expiry = resource.expires_at.is_some_and(|at| at <= now);
                let committed = resource.kind == ResourceKind::Host
                    && resource.releasable_at.is_some_and(|at| now < at);
                past_expiry && !committed
            })
            .map(|entry| entry.resource.id.clone())
            .collect();
        for id in &expired {
            inner.resources.remove(id);
        }
        inner.janitor_last_success = Some(now);
        expired
    }

    /// Withdraws the spot instances of `intent` without notice. Their disks
    /// and addresses go with them unless `leak_secondary_on_delete` is set.
    /// Returns the number of instances removed.
    pub fn interrupt_spot(&self, intent: &CloudIntentId) -> usize {
        let mut inner = self.lock();
        let group = intent.as_str().to_string();
        let instances: Vec<String> = inner
            .resources
            .values()
            .filter(|entry| {
                entry.group == group
                    && entry.resource.kind == ResourceKind::Instance
                    && entry.purchase == PurchaseModel::Spot
            })
            .map(|entry| entry.resource.id.clone())
            .collect();
        for id in &instances {
            inner.remove_instance(id);
        }
        instances.len()
    }

    /// Adds a resource the engine did not create, for orphan tests.
    pub fn inject_resource(&self, resource: OwnedResource) {
        let mut inner = self.lock();
        let group = resource
            .intent_id
            .as_ref()
            .map(|id| id.as_str().to_string())
            .unwrap_or_else(|| format!("injected:{}", resource.id));
        let ready_at = resource.created_at;
        inner.resources.insert(
            resource.id.clone(),
            SimResource {
                resource,
                purchase: PurchaseModel::OnDemand,
                ready_at,
                group,
            },
        );
    }

    /// Everything that exists right now, ignoring listing lag.
    pub fn snapshot(&self) -> Vec<OwnedResource> {
        let now = self.clock.now();
        self.lock()
            .resources
            .values()
            .map(|entry| entry.view(now))
            .collect()
    }

    /// Every `create` call received, in order, including failed ones.
    pub fn create_calls(&self) -> Vec<SimCreateCall> {
        self.lock().create_calls.clone()
    }

    /// The resource identifier of every `delete` call received, in order.
    pub fn delete_calls(&self) -> Vec<String> {
        self.lock().delete_calls.clone()
    }
}

impl SimResource {
    fn view(&self, now: UnixMillis) -> OwnedResource {
        let mut resource = self.resource.clone();
        if resource.kind == ResourceKind::Instance {
            resource.state = if now >= self.ready_at {
                ResourceState::Running
            } else {
                ResourceState::Pending
            };
        }
        resource
    }
}

impl Inner {
    fn visible(&self, entry: &SimResource, now: UnixMillis) -> bool {
        now >= entry
            .resource
            .created_at
            .plus_secs(self.faults.listing_lag_secs)
    }

    fn next_id(&mut self, kind: ResourceKind) -> String {
        let serial = self.next_serial;
        self.next_serial += 1;
        format!("sim-{}-{serial:06}", kind.as_str())
    }

    fn remove_instance(&mut self, id: &str) {
        let Some(instance) = self.resources.remove(id) else {
            return;
        };
        if self.faults.leak_secondary_on_delete {
            return;
        }
        let secondary: Vec<String> = self
            .resources
            .values()
            .filter(|entry| {
                entry.group == instance.group
                    && matches!(
                        entry.resource.kind,
                        ResourceKind::Disk | ResourceKind::Address
                    )
            })
            .map(|entry| entry.resource.id.clone())
            .collect();
        for id in secondary {
            self.resources.remove(&id);
        }
    }

    fn launch(
        &mut self,
        request: &CreateRequest,
        preset: &SimPreset,
        price: u64,
        now: UnixMillis,
    ) -> Vec<OwnedResource> {
        let group = request.intent_id.as_str().to_string();
        let ready_at = now.plus_secs(self.faults.readiness_delay_secs);
        let mut parts: Vec<(ResourceKind, u64, Option<UnixMillis>)> = Vec::new();
        if preset.dedicated_host {
            let releasable = now.plus_secs(preset.preset.minimum_commitment_secs);
            parts.push((ResourceKind::Host, price, Some(releasable)));
            parts.push((ResourceKind::Instance, 0, None));
        } else {
            parts.push((ResourceKind::Instance, price, None));
        }
        parts.push((ResourceKind::Disk, self.disk_hourly, None));
        parts.push((ResourceKind::Address, self.address_hourly, None));

        let mut created = Vec::new();
        for (kind, hourly_micros, releasable_at) in parts {
            let id = self.next_id(kind);
            let resource = OwnedResource {
                id: id.clone(),
                kind,
                state: ResourceState::Running,
                intent_id: request
                    .labels
                    .get(LABEL_INTENT)
                    .and_then(|text| CloudIntentId::parse(text).ok()),
                labels: request.labels.clone(),
                created_at: now,
                expires_at: Some(request.expires_at),
                releasable_at,
                hourly_micros,
            };
            let entry = SimResource {
                resource,
                purchase: request.purchase,
                ready_at,
                group: group.clone(),
            };
            created.push(entry.view(now));
            self.resources.insert(id, entry);
        }
        created
    }

    fn create(
        &mut self,
        request: &CreateRequest,
        now: UnixMillis,
        call: &mut SimCreateCall,
    ) -> Result<CreateAck, ProviderError> {
        if self.faults.unavailable {
            return Err(ProviderError::Unavailable);
        }
        let Some(preset) = self
            .presets
            .iter()
            .find(|p| p.preset.name == request.preset)
            .cloned()
        else {
            return Err(ProviderError::Rejected {
                reason: format!("unknown preset `{}`", request.preset),
            });
        };
        if !self.regions.contains(&request.region) {
            return Err(ProviderError::Rejected {
                reason: format!("unknown region `{}`", request.region),
            });
        }
        let Some(price) = preset.price(request.purchase) else {
            return Err(ProviderError::Rejected {
                reason: format!(
                    "preset `{}` is not sold as {}",
                    request.preset, request.purchase
                ),
            });
        };
        if let Some(error) = self.faults.fail_create.pop_front() {
            return Err(error);
        }
        if self.faults.quota_exceeded {
            return Err(ProviderError::QuotaExceeded);
        }
        if request.purchase == PurchaseModel::Spot && self.faults.spot_capacity_unavailable {
            return Err(ProviderError::CapacityUnavailable);
        }

        let group = request.intent_id.as_str().to_string();
        let known_token = self.tokens.contains(&group);
        let resources = if known_token && !self.faults.ignore_idempotency_token {
            self.resources
                .values()
                .filter(|entry| entry.group == group)
                .map(|entry| entry.view(now))
                .collect()
        } else {
            self.tokens.insert(group);
            call.created = true;
            self.launch(request, &preset, price, now)
        };
        if self.faults.lose_create_response > 0 {
            self.faults.lose_create_response -= 1;
            return Err(ProviderError::Timeout);
        }
        Ok(CreateAck { resources })
    }

    fn delete(&mut self, resource_id: &str, now: UnixMillis) -> Result<(), ProviderError> {
        if self.faults.unavailable {
            return Err(ProviderError::Unavailable);
        }
        let Some(entry) = self.resources.get(resource_id).cloned() else {
            return Ok(());
        };
        let kind = entry.resource.kind;
        if self.faults.undeletable.contains(&kind) {
            return Err(ProviderError::Unavailable);
        }
        if kind == ResourceKind::Host {
            if entry.resource.releasable_at.is_some_and(|at| now < at) {
                return Err(ProviderError::Rejected {
                    reason: "the host is under its minimum commitment".to_string(),
                });
            }
            let occupied = self.resources.values().any(|other| {
                other.group == entry.group && other.resource.kind == ResourceKind::Instance
            });
            if occupied {
                return Err(ProviderError::Rejected {
                    reason: "the host still has an instance".to_string(),
                });
            }
        }
        if kind == ResourceKind::Instance {
            self.remove_instance(resource_id);
        } else {
            self.resources.remove(resource_id);
        }
        if self.faults.lose_delete_response > 0 {
            self.faults.lose_delete_response -= 1;
            return Err(ProviderError::Timeout);
        }
        Ok(())
    }
}

impl Provider for SimProvider {
    fn capabilities(&self) -> Capabilities {
        let inner = self.lock();
        let mut purchase_models: Vec<PurchaseModel> = inner
            .presets
            .iter()
            .flat_map(|p| p.preset.purchase.iter().copied())
            .collect();
        purchase_models.sort();
        purchase_models.dedup();
        Capabilities {
            provider: inner.name.clone(),
            billing: inner.billing,
            currency: inner.currency.clone(),
            regions: inner.regions.clone(),
            presets: inner.presets.iter().map(|p| p.preset.clone()).collect(),
            purchase_models,
            provider_side_expiry: true,
        }
    }

    fn quote(
        &self,
        preset: &str,
        region: &str,
        purchase: PurchaseModel,
        lifetime_secs: u64,
    ) -> Result<Quote, ProviderError> {
        let now = self.clock.now();
        let inner = self.lock();
        if inner.faults.unavailable {
            return Err(ProviderError::Unavailable);
        }
        let Some(entry) = inner.presets.iter().find(|p| p.preset.name == preset) else {
            return Err(ProviderError::Rejected {
                reason: format!("unknown preset `{preset}`"),
            });
        };
        if !inner.regions.iter().any(|known| known == region) {
            return Err(ProviderError::Rejected {
                reason: format!("unknown region `{region}`"),
            });
        }
        let Some(price) = entry.price(purchase) else {
            return Err(ProviderError::Rejected {
                reason: format!("preset `{preset}` is not sold as {purchase}"),
            });
        };
        let overflow = || ProviderError::Rejected {
            reason: "price out of range".to_string(),
        };
        let hourly_micros = price
            .checked_add(inner.disk_hourly)
            .and_then(|sum| sum.checked_add(inner.address_hourly))
            .ok_or_else(overflow)?;
        let minimum_commitment_micros = entry
            .preset
            .minimum_commitment_secs
            .checked_mul(1000)
            .and_then(|millis| prorate_up(price, millis))
            .ok_or_else(overflow)?;
        let mut assumptions = vec![
            "simulated list price; no real provider was consulted".to_string(),
            "includes one boot disk and one address".to_string(),
        ];
        if purchase == PurchaseModel::Spot {
            assumptions.push(
                "spot price can change and capacity can be withdrawn without notice".to_string(),
            );
        }
        if minimum_commitment_micros > 0 {
            assumptions.push(format!(
                "the host is billed for at least {} seconds once created",
                entry.preset.minimum_commitment_secs
            ));
        }
        Ok(Quote {
            preset: preset.to_string(),
            region: region.to_string(),
            purchase,
            lifetime_secs,
            hourly_micros,
            currency: inner.currency.clone(),
            priced_at: now,
            minimum_commitment_micros,
            assumptions,
        })
    }

    fn create(&self, request: &CreateRequest) -> Result<CreateAck, ProviderError> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let mut call = SimCreateCall {
            intent: request.intent_id.clone(),
            purchase: request.purchase,
            at: now,
            created: false,
        };
        let outcome = inner.create(request, now, &mut call);
        inner.create_calls.push(call);
        outcome
    }

    fn find_by_intent(&self, intent: &CloudIntentId) -> Result<Vec<OwnedResource>, ProviderError> {
        let now = self.clock.now();
        let inner = self.lock();
        if inner.faults.unavailable {
            return Err(ProviderError::Unavailable);
        }
        Ok(inner
            .resources
            .values()
            .filter(|entry| entry.resource.intent_id.as_ref() == Some(intent))
            .filter(|entry| inner.visible(entry, now))
            .map(|entry| entry.view(now))
            .collect())
    }

    fn list_owned(&self, fleet: &FleetId) -> Result<Vec<OwnedResource>, ProviderError> {
        let now = self.clock.now();
        let inner = self.lock();
        if inner.faults.unavailable {
            return Err(ProviderError::Unavailable);
        }
        Ok(inner
            .resources
            .values()
            .filter(|entry| {
                let label = entry.resource.labels.get(LABEL_FLEET);
                label.map(String::as_str) == Some(fleet.as_str())
                    || (label.is_none() && inner.faults.list_unlabelled)
            })
            .filter(|entry| inner.visible(entry, now))
            .map(|entry| entry.view(now))
            .collect())
    }

    fn delete(&self, resource_id: &str) -> Result<(), ProviderError> {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.delete_calls.push(resource_id.to_string());
        inner.delete(resource_id, now)
    }

    fn janitor_status(&self) -> Result<JanitorStatus, ProviderError> {
        let inner = self.lock();
        if inner.faults.unavailable {
            return Err(ProviderError::Unavailable);
        }
        Ok(JanitorStatus {
            installed: inner.janitor_installed,
            last_success_at: inner.janitor_last_success,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::ManualClock;
    use std::time::Duration;

    fn request(intent: &CloudIntentId, fleet: &FleetId, expires_at: UnixMillis) -> CreateRequest {
        let mut labels = BTreeMap::new();
        labels.insert(LABEL_FLEET.to_string(), fleet.as_str().to_string());
        labels.insert(LABEL_INTENT.to_string(), intent.as_str().to_string());
        CreateRequest {
            intent_id: intent.clone(),
            fleet_id: fleet.clone(),
            preset: "linux-small".to_string(),
            region: "sim-east".to_string(),
            purchase: PurchaseModel::OnDemand,
            image: "image-1".to_string(),
            expires_at,
            labels,
            bootstrap: "enroll".to_string(),
        }
    }

    fn sim() -> (ManualClock, SimProvider) {
        let clock = ManualClock::at(UnixMillis(1_000_000));
        let sim = SimProvider::new(Arc::new(clock.clone()));
        (clock, sim)
    }

    #[test]
    fn create_is_idempotent_per_token_and_labels_every_resource() {
        let (clock, sim) = sim();
        let fleet = FleetId::generate();
        let intent = CloudIntentId::generate();
        let req = request(&intent, &fleet, clock.now().plus_secs(3600));
        let first = sim.create(&req).unwrap();
        let second = sim.create(&req).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.resources.len(), 3);
        assert!(
            first
                .resources
                .iter()
                .all(|r| r.intent_id.as_ref() == Some(&intent)
                    && r.labels.get(LABEL_FLEET).map(String::as_str) == Some(fleet.as_str()))
        );
        assert_eq!(sim.find_by_intent(&intent).unwrap().len(), 3);
        assert_eq!(sim.list_owned(&fleet).unwrap().len(), 3);
        assert_eq!(sim.list_owned(&FleetId::generate()).unwrap().len(), 0);
        let calls = sim.create_calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].created && !calls[1].created);
    }

    #[test]
    fn lost_responses_still_take_effect() {
        let (clock, sim) = sim();
        let fleet = FleetId::generate();
        let intent = CloudIntentId::generate();
        sim.faults(|f| f.lose_create_response = 1);
        let req = request(&intent, &fleet, clock.now().plus_secs(3600));
        assert_eq!(sim.create(&req), Err(ProviderError::Timeout));
        let found = sim.find_by_intent(&intent).unwrap();
        assert_eq!(found.len(), 3);

        sim.faults(|f| f.lose_delete_response = 1);
        let instance = found
            .iter()
            .find(|r| r.kind == ResourceKind::Instance)
            .unwrap();
        assert_eq!(sim.delete(&instance.id), Err(ProviderError::Timeout));
        assert!(sim.find_by_intent(&intent).unwrap().is_empty());
        assert_eq!(sim.delete(&instance.id), Ok(()));
    }

    #[test]
    fn readiness_listing_lag_and_leaks_follow_the_switches() {
        let (clock, sim) = sim();
        let fleet = FleetId::generate();
        let intent = CloudIntentId::generate();
        sim.faults(|f| {
            f.readiness_delay_secs = 30;
            f.listing_lag_secs = 10;
            f.leak_secondary_on_delete = true;
        });
        let ack = sim
            .create(&request(&intent, &fleet, clock.now().plus_secs(3600)))
            .unwrap();
        let instance = ack
            .resources
            .iter()
            .find(|r| r.kind == ResourceKind::Instance)
            .unwrap();
        assert_eq!(instance.state, ResourceState::Pending);
        assert!(sim.find_by_intent(&intent).unwrap().is_empty());
        clock.advance(Duration::from_secs(30));
        let listed = sim.find_by_intent(&intent).unwrap();
        assert!(
            listed
                .iter()
                .any(|r| r.kind == ResourceKind::Instance && r.state == ResourceState::Running)
        );
        sim.delete(&instance.id).unwrap();
        let left: Vec<ResourceKind> = sim
            .find_by_intent(&intent)
            .unwrap()
            .iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(left, vec![ResourceKind::Address, ResourceKind::Disk]);
    }

    #[test]
    fn the_janitor_removes_expired_resources_only_when_it_runs() {
        let (clock, sim) = sim();
        let fleet = FleetId::generate();
        let intent = CloudIntentId::generate();
        sim.create(&request(&intent, &fleet, clock.now().plus_secs(60)))
            .unwrap();
        clock.advance(Duration::from_secs(61));
        assert_eq!(sim.snapshot().len(), 3);

        sim.faults(|f| f.janitor_stalled = true);
        assert!(sim.tick().is_empty());
        sim.faults(|f| f.janitor_stalled = false);
        sim.set_janitor(false, None);
        assert!(sim.tick().is_empty());
        assert_eq!(sim.snapshot().len(), 3);

        sim.set_janitor(true, None);
        assert_eq!(sim.tick().len(), 3);
        assert!(sim.snapshot().is_empty());
        assert_eq!(
            sim.janitor_status().unwrap().last_success_at,
            Some(clock.now())
        );
    }

    #[test]
    fn a_dedicated_host_cannot_be_released_before_its_commitment_ends() {
        let (clock, sim) = sim();
        let fleet = FleetId::generate();
        let intent = CloudIntentId::generate();
        let mut req = request(&intent, &fleet, clock.now().plus_secs(3600));
        req.preset = "mac-dedicated".to_string();
        let ack = sim.create(&req).unwrap();
        let host = ack
            .resources
            .iter()
            .find(|r| r.kind == ResourceKind::Host)
            .unwrap();
        let instance = ack
            .resources
            .iter()
            .find(|r| r.kind == ResourceKind::Instance)
            .unwrap();
        assert_eq!(instance.hourly_micros, 0);
        sim.delete(&instance.id).unwrap();
        assert!(matches!(
            sim.delete(&host.id),
            Err(ProviderError::Rejected { .. })
        ));
        clock.advance(Duration::from_secs(2 * 3600));
        sim.tick();
        assert_eq!(sim.snapshot().len(), 1, "only the committed host remains");
        clock.advance(Duration::from_secs(22 * 3600));
        assert_eq!(sim.delete(&host.id), Ok(()));
        assert!(sim.snapshot().is_empty());
    }

    #[test]
    fn quotes_follow_price_changes_and_carry_the_commitment() {
        let (clock, sim) = sim();
        let quote = sim
            .quote("linux-small", "sim-east", PurchaseModel::Spot, 3600)
            .unwrap();
        assert_eq!(quote.hourly_micros, 60_000 + 4_000 + 5_000);
        assert_eq!(quote.priced_at, clock.now());
        assert_eq!(quote.minimum_commitment_micros, 0);
        assert!(sim.set_price("linux-small", PurchaseModel::Spot, 90_000));
        let quote = sim
            .quote("linux-small", "sim-east", PurchaseModel::Spot, 3600)
            .unwrap();
        assert_eq!(quote.hourly_micros, 99_000);

        let host = sim
            .quote("mac-dedicated", "sim-east", PurchaseModel::OnDemand, 3600)
            .unwrap();
        assert_eq!(host.minimum_commitment_micros, 24 * 1_200_000);
        assert!(matches!(
            sim.quote("mac-dedicated", "sim-east", PurchaseModel::Spot, 3600),
            Err(ProviderError::Rejected { .. })
        ));
        assert!(!sim.set_price("mac-dedicated", PurchaseModel::Spot, 1));
    }
}
