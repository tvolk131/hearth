//! Authenticated, two-controller stopped-copy handoff.  Every durable ownership
//! decision is local; an expired grant or a lost acknowledgement never authorizes
//! the source to start again. Physical effects are performed by the scoped helper.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use iroh::{PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::protocol::{Guest, Vm};

pub const CHUNK_BYTES: usize = 128 * 1024;
pub const MAX_MIGRATION_BYTES: u64 = 16 * 1024 * 1024 * 1024 * 1024;
pub const MAX_GRANT_SECONDS: u64 = 24 * 60 * 60;

pub fn digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("serializable migration value"))
    )
}
fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub server_id: String,
    pub environment: String,
}
impl Peer {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.server_id.parse::<PublicKey>().is_ok() && hex(&self.environment, 32),
            "invalid migration peer identity"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: u32,
    pub migration_id: String,
    pub source: Peer,
    pub destination: Peer,
    pub source_vm_id: u16,
    pub target_vm_id: u16,
    pub source_owner: String,
    pub target_owner: String,
    pub source_machine_sha256: String,
    pub source_config_sha256: String,
    pub name: String,
    pub disk_mib: u64,
    pub memory_mib: u64,
    pub min_memory_mib: u64,
    pub cores: u16,
    pub guest: Option<Box<Guest>>,
    pub byte_ceiling: u64,
    pub target_bridge: String,
}
impl Plan {
    pub fn validate(&self) -> Result<()> {
        self.source.validate()?;
        self.destination.validate()?;
        ensure!(
            self.schema == 1 && hex(&self.migration_id, 64),
            "invalid migration plan version/id"
        );
        ensure!(
            self.source.server_id != self.destination.server_id
                && self.source.environment != self.destination.environment,
            "migration requires independent controller identities"
        );
        ensure!(
            (9001..=9099).contains(&self.source_vm_id)
                && (9001..=9099).contains(&self.target_vm_id),
            "migration VMID outside supported guest range"
        );
        ensure!(
            label(&self.source_owner) && label(&self.target_owner) && label(&self.name),
            "invalid migration owner/name"
        );
        ensure!(
            hex(&self.source_machine_sha256, 64) && hex(&self.source_config_sha256, 64),
            "source identity/configuration fingerprints missing"
        );
        ensure!(
            (64..=16 * 1024 * 1024).contains(&self.disk_mib)
                && self.disk_mib.checked_mul(1024 * 1024).is_some(),
            "migration disk size outside bounds"
        );
        ensure!(
            (128..=1024 * 1024).contains(&self.min_memory_mib)
                && self.min_memory_mib <= self.memory_mib
                && self.memory_mib <= 1024 * 1024
                && (1..=256).contains(&self.cores),
            "invalid migration resource profile"
        );
        ensure!(
            self.byte_ceiling >= (self.disk_mib + 4) * 1024 * 1024
                && self.byte_ceiling <= MAX_MIGRATION_BYTES,
            "invalid migration byte ceiling"
        );
        ensure!(
            self.target_bridge == "vmbr77",
            "initial migration adapter requires the isolated managed bridge"
        );
        if let Some(guest) = &self.guest {
            guest.validate()?;
        }
        Ok(())
    }
    pub fn vm(&self, role: Role) -> Vm {
        Vm {
            vm_id: if role == Role::Source {
                self.source_vm_id
            } else {
                self.target_vm_id
            },
            owner: if role == Role::Source {
                self.source_owner.clone()
            } else {
                self.target_owner.clone()
            },
            name: self.name.clone(),
            disk_mib: self.disk_mib,
            memory_mib: self.memory_mib,
            min_memory_mib: self.min_memory_mib,
            cores: self.cores,
            state: super::protocol::VmState::Stopped,
            guest: self.guest.clone(),
        }
    }
    pub fn peer(&self, role: Role) -> &Peer {
        if role == Role::Source {
            &self.source
        } else {
            &self.destination
        }
    }
    pub fn other(&self, role: Role) -> &Peer {
        self.peer(role.other())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Source,
    Destination,
}
impl Role {
    pub fn other(self) -> Self {
        if self == Self::Source {
            Self::Destination
        } else {
            Self::Source
        }
    }
}

/// Short-lived local administrator approval. The source's approval and the
/// destination's approval are separate signatures over the same exact plan.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub schema: u32,
    pub plan: Plan,
    pub role: Role,
    pub issuer: Peer,
    pub recipient: Peer,
    pub actor: String,
    pub nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_grant_sha256: Option<String>,
    pub issued_at: u64,
    pub expires_at: u64,
}
impl Grant {
    pub fn validate(&self, now: u64, allow_expired: bool) -> Result<()> {
        self.plan.validate()?;
        ensure!(
            self.schema == 1
                && self.issuer == *self.plan.peer(self.role)
                && self.recipient == *self.plan.other(self.role),
            "grant issuer/recipient/role does not match plan"
        );
        ensure!(
            self.actor.parse::<PublicKey>().is_ok() && hex(&self.nonce, 64),
            "grant administrator/renewal identity missing"
        );
        ensure!(
            match self.role {
                Role::Source => self.source_grant_sha256.is_none(),
                Role::Destination => self
                    .source_grant_sha256
                    .as_ref()
                    .is_some_and(|v| hex(v, 64)),
            },
            "destination approval must bind the exact source authorization"
        );
        ensure!(
            self.issued_at <= now.saturating_add(30)
                && self.expires_at > self.issued_at
                && self.expires_at - self.issued_at <= MAX_GRANT_SECONDS,
            "invalid grant lifetime"
        );
        ensure!(
            allow_expired || now < self.expires_at,
            "migration authorization expired; resources remain reserved and fenced"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Signed<T> {
    pub value: T,
    pub signature: String,
}
impl<T: Serialize> Signed<T> {
    pub fn sign(value: T, key: &SecretKey, domain: &str) -> Result<Self> {
        let bytes = signing_bytes(domain, &value)?;
        Ok(Self {
            value,
            signature: STANDARD.encode(key.sign(&bytes).to_bytes()),
        })
    }
    pub fn verify(&self, issuer: &str, domain: &str) -> Result<()> {
        let bytes: [u8; 64] = STANDARD
            .decode(&self.signature)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid signature length"))?;
        let public = issuer.parse::<PublicKey>()?;
        public.verify(
            &signing_bytes(domain, &self.value)?,
            &Signature::from_bytes(&bytes),
        )?;
        Ok(())
    }
}
fn signing_bytes(domain: &str, value: &impl Serialize) -> Result<Vec<u8>> {
    ensure!(
        matches!(
            domain,
            "grant" | "manifest" | "ready" | "handoff" | "accepted" | "abort"
        ),
        "unknown migration signature domain"
    );
    let mut bytes = format!("pve-dev/migration/{domain}/1\0").into_bytes();
    bytes.extend(serde_json::to_vec(value)?);
    Ok(bytes)
}
impl Signed<Grant> {
    pub fn validate_grant(&self, now: u64, allow_expired: bool) -> Result<()> {
        self.value.validate(now, allow_expired)?;
        self.verify(&self.value.issuer.server_id, "grant")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VolumeManifest {
    pub bytes: u64,
    pub sha256: String,
    pub guid: String,
    pub createtxg: String,
}
impl VolumeManifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.bytes > 0 && self.bytes <= MAX_MIGRATION_BYTES && hex(&self.sha256, 64),
            "invalid volume bytes/hash"
        );
        ensure!(
            !self.guid.is_empty()
                && self.guid.len() <= 20
                && self.guid.bytes().all(|b| b.is_ascii_digit())
                && !self.createtxg.is_empty()
                && self.createtxg.len() <= 20
                && self.createtxg.bytes().all(|b| b.is_ascii_digit()),
            "invalid volume physical identity"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plan_sha256: String,
    pub source_fence: Value,
    pub config: BTreeMap<String, String>,
    pub volumes: BTreeMap<String, VolumeManifest>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snippets: BTreeMap<String, VolumeManifest>,
}
impl Manifest {
    pub fn validate(&self, plan: &Plan) -> Result<()> {
        ensure!(
            self.plan_sha256 == digest(plan) && self.source_fence.is_object(),
            "manifest plan/fence proof mismatch"
        );
        ensure!(
            self.volumes.keys().cloned().collect::<BTreeSet<_>>()
                == BTreeSet::from(["cloudinit".into(), "root".into()]),
            "manifest must contain exactly root and cloud-init volumes"
        );
        ensure!(
            self.volumes["root"].bytes == plan.disk_mib * 1024 * 1024
                && self.volumes["cloudinit"].bytes == 4 * 1024 * 1024,
            "manifest volume sizes differ from authorized plan"
        );
        ensure!(
            self.snippets.len() <= 3
                && self
                    .snippets
                    .keys()
                    .all(|key| matches!(key.as_str(), "meta" | "user" | "network")),
            "unsupported snippet dependency"
        );
        let mut total = 0u64;
        for v in self.volumes.values().chain(self.snippets.values()) {
            v.validate()?;
            total = total
                .checked_add(v.bytes)
                .context("manifest byte overflow")?;
        }
        ensure!(
            total <= plan.byte_ceiling && self.snippets.values().all(|v| v.bytes <= 64 * 1024),
            "manifest exceeds authorized byte ceiling"
        );
        ensure!(
            self.config.len() <= 80
                && self
                    .config
                    .iter()
                    .all(|(k, v)| k.len() <= 64 && v.len() <= 8192)
                && digest(&self.config) == plan.source_config_sha256,
            "manifest configuration differs from approved source snapshot"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReadyReceipt {
    pub migration_id: String,
    pub plan_sha256: String,
    pub manifest_sha256: String,
    pub destination: Peer,
    pub target_identities: Value,
    pub destination_grant_sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AbortReceipt {
    pub migration_id: String,
    pub plan_sha256: String,
    pub source: Peer,
    pub aborted_at: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Acceptance {
    pub migration_id: String,
    pub plan_sha256: String,
    pub handoff_sha256: String,
    pub destination: Peer,
    pub accepted_at: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub migration_id: String,
    pub plan_sha256: String,
    pub manifest_sha256: String,
    pub ready_sha256: String,
    pub source: Peer,
    pub destination: Peer,
    pub source_grant_sha256: String,
    pub destination_grant_sha256: String,
    pub committed_at: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Authorized,
    Preparing,
    Fenced,
    Receiving,
    Ready,
    Relinquished,
    Accepted,
    Cleaning,
    Complete,
    Aborted,
    Blocked,
}
impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Aborted)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRecord {
    pub plan: Plan,
    pub role: Role,
    pub phase: Phase,
    pub source_grant: Signed<Grant>,
    pub destination_grant: Option<Signed<Grant>>,
    pub manifest: Option<Signed<Manifest>>,
    pub ready: Option<Signed<ReadyReceipt>>,
    pub handoff: Option<Signed<Handoff>>,
    pub acceptance: Option<Signed<Acceptance>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_proof: Option<Signed<AbortReceipt>>,
    pub offsets: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub renewals: BTreeMap<String, RenewalRecord>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seal_requested: bool,
    pub helper_proof: Option<Value>,
    pub peer_route: Option<super::transport::Descriptor>,
    pub actor: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub error: Option<String>,
}
impl TransferRecord {
    pub fn source(
        plan: Plan,
        grant: Signed<Grant>,
        actor: String,
        route: super::transport::Descriptor,
        now: u64,
    ) -> Result<Self> {
        grant.validate_grant(now, false)?;
        ensure!(
            grant.value.plan == plan
                && grant.value.role == Role::Source
                && actor == grant.value.actor,
            "source approval mismatch"
        );
        let record = Self {
            plan,
            role: Role::Source,
            phase: Phase::Authorized,
            source_grant: grant,
            destination_grant: None,
            manifest: None,
            ready: None,
            handoff: None,
            acceptance: None,
            abort_proof: None,
            offsets: BTreeMap::new(),
            renewals: BTreeMap::new(),
            seal_requested: false,
            helper_proof: None,
            peer_route: Some(route),
            actor,
            created_at: now,
            updated_at: now,
            error: None,
        };
        record.validate()?;
        Ok(record)
    }
    pub fn destination(
        source_grant: Signed<Grant>,
        grant: Signed<Grant>,
        actor: String,
        now: u64,
    ) -> Result<Self> {
        source_grant.validate_grant(now, false)?;
        grant.validate_grant(now, false)?;
        let plan = source_grant.value.plan.clone();
        ensure!(
            grant.value.plan == plan
                && source_grant.value.role == Role::Source
                && grant.value.role == Role::Destination
                && grant.value.source_grant_sha256.as_deref()
                    == Some(digest(&source_grant).as_str())
                && actor == grant.value.actor,
            "independent destination approval mismatch"
        );
        let record = Self {
            plan,
            role: Role::Destination,
            phase: Phase::Authorized,
            source_grant,
            destination_grant: Some(grant),
            manifest: None,
            ready: None,
            handoff: None,
            acceptance: None,
            abort_proof: None,
            offsets: BTreeMap::new(),
            renewals: BTreeMap::new(),
            seal_requested: false,
            helper_proof: None,
            peer_route: None,
            actor,
            created_at: now,
            updated_at: now,
            error: None,
        };
        record.validate()?;
        Ok(record)
    }
    pub fn validate(&self) -> Result<()> {
        self.plan.validate()?;
        self.source_grant
            .validate_grant(self.source_grant.value.issued_at, true)?;
        ensure!(
            self.source_grant.value.role == Role::Source
                && self.source_grant.value.plan == self.plan,
            "source approval changed"
        );
        if let Some(grant) = &self.destination_grant {
            grant.validate_grant(grant.value.issued_at, true)?;
            ensure!(
                grant.value.role == Role::Destination
                    && grant.value.plan == self.plan
                    && grant.value.source_grant_sha256.as_deref()
                        == Some(digest(&self.source_grant).as_str()),
                "destination approval changed or binds another source authorization"
            );
        }
        ensure!(
            self.role == Role::Source || self.destination_grant.is_some(),
            "destination has no independent authorization"
        );
        if let Some(manifest) = &self.manifest {
            manifest.verify(&self.plan.source.server_id, "manifest")?;
            manifest.value.validate(&self.plan)?;
        }
        if let Some(ready) = &self.ready {
            ready.verify(&self.plan.destination.server_id, "ready")?;
            ensure!(
                ready.value.migration_id == self.plan.migration_id
                    && ready.value.plan_sha256 == digest(&self.plan)
                    && ready.value.destination == self.plan.destination
                    && ready.value.target_identities.is_object(),
                "readiness identity mismatch"
            );
            ensure!(
                self.manifest
                    .as_ref()
                    .is_some_and(|m| ready.value.manifest_sha256 == digest(&m.value))
                    && self
                        .destination_grant
                        .as_ref()
                        .is_some_and(|g| ready.value.destination_grant_sha256 == digest(g)),
                "readiness manifest/authorization mismatch"
            );
        }
        if let Some(handoff) = &self.handoff {
            self.validate_handoff(handoff)?;
        }
        if let Some(acceptance) = &self.acceptance {
            self.validate_acceptance(acceptance)?;
        }
        ensure!(
            self.phase != Phase::Complete || self.acceptance.is_some(),
            "completed transfer lacks destination acceptance"
        );
        ensure!(
            !matches!(
                self.phase,
                Phase::Ready | Phase::Relinquished | Phase::Accepted | Phase::Complete
            ) || self.ready.is_some(),
            "readiness proof missing"
        );
        ensure!(
            !matches!(
                self.phase,
                Phase::Relinquished | Phase::Accepted | Phase::Complete
            ) || self.handoff.is_some(),
            "ownership decision missing"
        );
        ensure!(
            self.phase != Phase::Aborted || self.handoff.is_none(),
            "committed transfer cannot abort"
        );
        if let Some(abort) = &self.abort_proof {
            self.validate_abort_proof(abort)?;
        }
        ensure!(
            self.phase != Phase::Aborted || self.role == Role::Source || self.abort_proof.is_some(),
            "destination abort requires durable signed source cancellation"
        );
        ensure!(
            self.renewals.len() <= 32,
            "migration renewal history exceeds bound"
        );
        for (id, renewal) in &self.renewals {
            ensure!(
                !id.is_empty()
                    && id.len() <= 96
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                    && hex(&renewal.fingerprint, 64),
                "invalid renewal request identity"
            );
            renewal
                .grant
                .validate_grant(renewal.grant.value.issued_at, true)?;
            ensure!(
                renewal.grant.value.plan == self.plan && renewal.grant.value.role == self.role,
                "renewal receipt belongs to another plan/role"
            );
        }
        if let Some(route) = &self.peer_route {
            ensure!(
                route.environment == self.plan.other(self.role).environment
                    && route.endpoint.id.to_string() == self.plan.other(self.role).server_id,
                "peer route authority differs from immutable plan"
            );
        }
        if let Some(manifest) = &self.manifest {
            for (slot, offset) in &self.offsets {
                let volume = manifest
                    .value
                    .volumes
                    .get(slot)
                    .or_else(|| manifest.value.snippets.get(slot))
                    .context("offset has unknown slot")?;
                ensure!(
                    *offset <= volume.bytes,
                    "received offset exceeds authorized volume"
                );
            }
        } else {
            ensure!(
                self.offsets.is_empty(),
                "offsets exist without a final stopped manifest"
            );
        }
        Ok(())
    }
    pub fn validate_handoff(&self, signed: &Signed<Handoff>) -> Result<()> {
        signed.verify(&self.plan.source.server_id, "handoff")?;
        let h = &signed.value;
        ensure!(
            h.migration_id == self.plan.migration_id
                && h.plan_sha256 == digest(&self.plan)
                && h.source == self.plan.source
                && h.destination == self.plan.destination,
            "handoff plan/peer mismatch"
        );
        ensure!(
            self.manifest
                .as_ref()
                .is_some_and(|m| h.manifest_sha256 == digest(&m.value))
                && self
                    .ready
                    .as_ref()
                    .is_some_and(|r| h.ready_sha256 == digest(&r.value)),
            "handoff does not bind final readiness/manifest"
        );
        ensure!(
            h.source_grant_sha256 == digest(&self.source_grant)
                && self
                    .destination_grant
                    .as_ref()
                    .is_some_and(|g| h.destination_grant_sha256 == digest(g)
                        && h.committed_at < g.value.expires_at)
                && h.committed_at < self.source_grant.value.expires_at
                && h.committed_at >= self.source_grant.value.issued_at,
            "handoff authorization mismatch/late commit"
        );
        Ok(())
    }
    pub fn validate_acceptance(&self, signed: &Signed<Acceptance>) -> Result<()> {
        signed.verify(&self.plan.destination.server_id, "accepted")?;
        let a = &signed.value;
        ensure!(
            a.migration_id == self.plan.migration_id
                && a.plan_sha256 == digest(&self.plan)
                && a.destination == self.plan.destination
                && self
                    .handoff
                    .as_ref()
                    .is_some_and(|h| a.handoff_sha256 == digest(&h.value)
                        && a.accepted_at >= h.value.committed_at),
            "destination acceptance does not bind ownership proof"
        );
        Ok(())
    }
    pub fn acknowledge(&mut self, signed: Signed<Acceptance>, now: u64) -> Result<()> {
        self.validate_acceptance(&signed)?;
        if let Some(old) = &self.acceptance {
            ensure!(old == &signed, "acceptance cannot change");
        }
        self.acceptance = Some(signed);
        self.updated_at = now;
        self.validate()
    }
    pub fn validate_abort_proof(&self, signed: &Signed<AbortReceipt>) -> Result<()> {
        signed.verify(&self.plan.source.server_id, "abort")?;
        let value = &signed.value;
        ensure!(
            value.migration_id == self.plan.migration_id
                && value.plan_sha256 == digest(&self.plan)
                && value.source == self.plan.source
                && value.aborted_at >= self.source_grant.value.issued_at,
            "source cancellation proof differs from transfer"
        );
        ensure!(
            self.handoff.is_none(),
            "source cancellation cannot replace committed ownership"
        );
        Ok(())
    }
    pub fn fresh(&self, now: u64) -> Result<()> {
        self.source_grant.validate_grant(now, false)?;
        self.destination_grant
            .as_ref()
            .context("destination not authorized")?
            .validate_grant(now, false)
    }
    pub fn set_destination_grant(&mut self, grant: Signed<Grant>, now: u64) -> Result<()> {
        ensure!(
            self.role == Role::Source && matches!(self.phase, Phase::Authorized | Phase::Fenced),
            "source is no longer awaiting destination approval"
        );
        grant.validate_grant(now, false)?;
        ensure!(
            grant.value.role == Role::Destination
                && grant.value.plan == self.plan
                && grant.value.source_grant_sha256.as_deref()
                    == Some(digest(&self.source_grant).as_str()),
            "destination approved another plan or source authorization"
        );
        if let Some(old) = &self.destination_grant {
            ensure!(old == &grant, "destination approval cannot be replaced");
        }
        self.destination_grant = Some(grant);
        self.updated_at = now;
        self.validate()
    }
    pub fn set_manifest(&mut self, manifest: Signed<Manifest>, now: u64) -> Result<()> {
        self.fresh(now)?;
        manifest.verify(&self.plan.source.server_id, "manifest")?;
        manifest.value.validate(&self.plan)?;
        ensure!(
            matches!(
                self.phase,
                Phase::Authorized | Phase::Preparing | Phase::Fenced | Phase::Receiving
            ),
            "transfer cannot accept a new stopped manifest in this phase"
        );
        if let Some(old) = &self.manifest {
            ensure!(old == &manifest, "stopped manifest is immutable");
        }
        self.manifest = Some(manifest);
        self.phase = if self.role == Role::Source {
            Phase::Fenced
        } else {
            Phase::Receiving
        };
        self.updated_at = now;
        self.validate()
    }
    pub fn set_ready(&mut self, ready: Signed<ReadyReceipt>, now: u64) -> Result<()> {
        self.fresh(now)?;
        ensure!(
            matches!(self.phase, Phase::Fenced | Phase::Receiving | Phase::Ready),
            "transfer is not receiving/fenced"
        );
        if let Some(old) = &self.ready {
            ensure!(old == &ready, "readiness receipt is immutable");
        }
        let mut candidate = self.clone();
        candidate.ready = Some(ready);
        candidate.phase = Phase::Ready;
        candidate.updated_at = now;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }
    /// Store this state and signature in one durable transaction BEFORE sending
    /// the proof. Timeout/expiry thereafter must never restore source authority.
    pub fn relinquish(&mut self, key: &SecretKey, now: u64) -> Result<Signed<Handoff>> {
        ensure!(
            self.role == Role::Source && key.public().to_string() == self.plan.source.server_id,
            "only bound source can relinquish"
        );
        if let Some(proof) = &self.handoff {
            self.validate_handoff(proof)?;
            return Ok(proof.clone());
        }
        self.fresh(now)?;
        ensure!(
            self.phase == Phase::Ready,
            "source has no verified destination readiness"
        );
        let proof = Signed::sign(
            Handoff {
                migration_id: self.plan.migration_id.clone(),
                plan_sha256: digest(&self.plan),
                manifest_sha256: digest(&self.manifest.as_ref().context("manifest missing")?.value),
                ready_sha256: digest(&self.ready.as_ref().context("readiness missing")?.value),
                source: self.plan.source.clone(),
                destination: self.plan.destination.clone(),
                source_grant_sha256: digest(&self.source_grant),
                destination_grant_sha256: digest(
                    self.destination_grant
                        .as_ref()
                        .context("destination grant missing")?,
                ),
                committed_at: now,
            },
            key,
            "handoff",
        )?;
        self.handoff = Some(proof.clone());
        self.phase = Phase::Relinquished;
        self.updated_at = now;
        self.validate()?;
        Ok(proof)
    }
    pub fn accept(&mut self, proof: Signed<Handoff>, now: u64) -> Result<()> {
        ensure!(
            self.role == Role::Destination,
            "only destination accepts ownership"
        );
        self.validate_handoff(&proof)?;
        if let Some(old) = &self.handoff {
            ensure!(
                old == &proof,
                "different ownership proof cannot replace committed proof"
            );
        }
        ensure!(
            matches!(self.phase, Phase::Ready | Phase::Accepted),
            "destination is not ready or already aborted"
        );
        self.handoff = Some(proof);
        self.phase = Phase::Accepted;
        self.updated_at = now;
        self.validate()
    }
    pub fn abort(&mut self, now: u64) -> Result<()> {
        ensure!(
            self.handoff.is_none()
                && !matches!(
                    self.phase,
                    Phase::Relinquished | Phase::Accepted | Phase::Complete
                ),
            "committed ownership cannot be canceled or rolled back"
        );
        let mut candidate = self.clone();
        candidate.phase = Phase::Aborted;
        candidate.updated_at = now;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }
    pub fn holds_vm(&self, owner: &str, vm_id: u16) -> bool {
        let local_owner = if self.role == Role::Source {
            &self.plan.source_owner
        } else {
            &self.plan.target_owner
        };
        let local_id = if self.role == Role::Source {
            self.plan.source_vm_id
        } else {
            self.plan.target_vm_id
        };
        local_owner == owner
            && local_id == vm_id
            && match self.role {
                Role::Source => !self.phase.terminal(),
                Role::Destination => !matches!(self.phase, Phase::Complete | Phase::Aborted),
            }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenewalRecord {
    pub fingerprint: String,
    pub grant: Signed<Grant>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminRequest {
    Describe {},
    Plan {
        request_id: String,
        vm_id: u16,
        destination: super::transport::Descriptor,
        target_owner: String,
        target_vm_id: u16,
        expires_in_seconds: u64,
    },
    AuthorizeDestination {
        source_grant: Signed<Grant>,
        expires_in_seconds: u64,
    },
    Start {
        migration_id: String,
        destination_grant: Signed<Grant>,
    },
    Status {
        migration_id: String,
    },
    Renew {
        migration_id: String,
        request_id: String,
        expires_in_seconds: u64,
        #[serde(default)]
        source_grant: Option<Signed<Grant>>,
    },
    List {},
    Abort {
        migration_id: String,
        confirm_cleanup: bool,
        #[serde(default)]
        source_abort: Option<Signed<AbortReceipt>>,
    },
    Finish {
        migration_id: String,
        confirm_guest_verified: bool,
        confirm_delete_recovery: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PeerMessage {
    Manifest {
        manifest: Signed<Manifest>,
    },
    Chunk {
        slot: String,
        offset: u64,
        bytes_base64: String,
        sha256: String,
        #[serde(default)]
        zero_bytes: Option<u64>,
    },
    Seal {},
    Accept {
        handoff: Signed<Handoff>,
    },
    Status {},
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerRequest {
    pub schema: u32,
    pub destination_environment: String,
    pub grant: Signed<Grant>,
    pub message: PeerMessage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMigrationRequest {
    InspectVm {
        vm_id: u16,
    },
    Prepare {
        plan: Plan,
        role: Role,
        #[serde(default)]
        manifest: Option<Manifest>,
    },
    Fence {
        migration_id: String,
    },
    ReadChunk {
        migration_id: String,
        slot: String,
        offset: u64,
    },
    WriteChunk {
        migration_id: String,
        slot: String,
        offset: u64,
        bytes_base64: String,
        sha256: String,
        #[serde(default)]
        zero_bytes: Option<u64>,
    },
    Seal {
        migration_id: String,
        manifest: Manifest,
    },
    Activate {
        migration_id: String,
        handoff: Signed<Handoff>,
    },
    Commit {
        migration_id: String,
        handoff: Signed<Handoff>,
    },
    Inspect {
        migration_id: String,
    },
    Cleanup {
        migration_id: String,
        confirm: bool,
        handoff: Signed<Handoff>,
        acceptance: Signed<Acceptance>,
    },
    Abort {
        migration_id: String,
        confirm_cleanup: bool,
        #[serde(default)]
        source_abort: Option<Signed<AbortReceipt>>,
        #[serde(default)]
        plan: Option<Plan>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub vm: Vm,
    pub machine_sha256: String,
    pub config_sha256: String,
    pub config: BTreeMap<String, String>,
    pub volume_identities: BTreeMap<String, Value>,
    pub byte_ceiling: u64,
}

/// The helper's own immutable admission/fence and transfer progress. Its private
/// journal is authoritative for physical effects; controller receipts alone do
/// not replace a matching physical machine identity.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostTransferRecord {
    pub plan: Plan,
    pub role: Role,
    pub phase: Phase,
    pub machine_before: super::host::MachineRecord,
    pub config_before: BTreeMap<String, String>,
    pub volumes_before: BTreeMap<String, Value>,
    pub manifest: Option<Manifest>,
    pub offsets: BTreeMap<String, u64>,
    pub last_chunk: Option<ChunkWitness>,
    pub target_identities: Option<Value>,
    pub handoff: Option<Signed<Handoff>>,
    pub error: Option<String>,
    pub native: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChunkWitness {
    pub slot: String,
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
}

pub fn validate_records(
    records: &BTreeMap<String, TransferRecord>,
    local_environment: &str,
) -> Result<()> {
    let mut active = BTreeSet::new();
    for (id, r) in records {
        r.validate()?;
        ensure!(
            id == &r.plan.migration_id && r.plan.peer(r.role).environment == local_environment,
            "migration journal/controller binding mismatch"
        );
        if !r.phase.terminal() {
            let vm = if r.role == Role::Source {
                r.plan.source_vm_id
            } else {
                r.plan.target_vm_id
            };
            ensure!(
                active.insert(vm),
                "multiple transfers retain the same local VM"
            );
        }
    }
    Ok(())
}
