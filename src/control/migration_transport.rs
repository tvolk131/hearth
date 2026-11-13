//! Migration-only authenticated iroh service. Public control RPC is not exposed
//! by this endpoint: only plan/grant-bound peer messages use this ALPN.
use super::{
    host::{self, HostRequest},
    migration::*,
    protocol::{AccountState, OperationStatus, Role as UserRole, User, VmState},
    store::{Catalog, Job, Record, Store, failure, now},
    transport::{Descriptor, Network},
};
use anyhow::{Context, Result, bail, ensure};
use iroh::{Endpoint, SecretKey, endpoint::presets};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::Semaphore, task::JoinSet, time::timeout};

pub const ALPN: &[u8] = b"pve-dev/migration/1";
pub const MAX_FRAME: usize = 256 * 1024;

pub async fn endpoint(identity: SecretKey, network: Network, port: u16) -> Result<Endpoint> {
    let mut builder = match network {
        Network::Public => Endpoint::builder(presets::N0),
        _ => Endpoint::builder(presets::Minimal),
    }
    .secret_key(identity)
    .alpns(vec![ALPN.to_vec()]);
    if network != Network::Public {
        builder = builder
            .clear_ip_transports()
            .clear_relay_transports()
            .clear_address_lookup()
            .bind_addr(
                format!(
                    "{}:{port}",
                    if network == Network::Loopback {
                        "127.0.0.1"
                    } else {
                        "0.0.0.0"
                    }
                )
                .parse::<std::net::SocketAddr>()?,
            )?;
    }
    Ok(builder.bind().await?)
}

pub struct Service {
    pub store: Arc<Store>,
    identity: SecretKey,
    host_socket: PathBuf,
    endpoint: Endpoint,
    descriptor: Descriptor,
    coordinator: Semaphore,
    connections: tokio::sync::Mutex<BTreeMap<String, iroh::endpoint::Connection>>,
}
impl Service {
    pub async fn bind(
        store: Arc<Store>,
        identity: SecretKey,
        host_socket: PathBuf,
        network: Network,
        port: u16,
    ) -> Result<Arc<Self>> {
        let endpoint = endpoint(identity.clone(), network, port).await?;
        let descriptor = Descriptor {
            schema: 1,
            endpoint: endpoint.addr(),
            environment: store.read()?.binding.environment,
            network,
        };
        Ok(Arc::new(Self {
            store,
            identity,
            host_socket,
            endpoint,
            descriptor,
            coordinator: Semaphore::new(1),
            connections: tokio::sync::Mutex::new(BTreeMap::new()),
        }))
    }
    pub fn describe(&self) -> Descriptor {
        Descriptor {
            endpoint: self.endpoint.addr(),
            ..self.descriptor.clone()
        }
    }
    pub fn public_peer(&self) -> Peer {
        Peer {
            server_id: self.identity.public().to_string(),
            environment: self.descriptor.environment.clone(),
        }
    }
    fn actor(&self, peer: &str) -> Result<User> {
        let c = self.store.read()?;
        let device = c
            .devices
            .get(peer)
            .filter(|d| !d.revoked)
            .context("administrator device not enrolled")?;
        let actor = c
            .users
            .get(&device.user_id)
            .filter(|u| u.ready && u.state == AccountState::Active && u.role == UserRole::Admin)
            .context("active administrator permission required")?;
        Ok(actor.clone())
    }
    fn record(&self, id: &str) -> Result<TransferRecord> {
        self.store
            .read()?
            .migrations
            .get(id)
            .cloned()
            .context("unknown migration")
    }
    fn change<T>(
        &self,
        id: &str,
        action: impl FnOnce(&mut TransferRecord) -> Result<T>,
    ) -> Result<T> {
        self.store
            .change(|c| {
                let record = c
                    .migrations
                    .get_mut(id)
                    .ok_or_else(|| failure("invalid", "unknown migration"))?;
                let result = action(record).map_err(|e| failure("conflict", &e.to_string()))?;
                record
                    .validate()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                Ok(result)
            })
            .map_err(Into::into)
    }
    async fn helper(&self, request: HostMigrationRequest) -> Result<Value> {
        let response = host::rpc(
            &self.host_socket,
            &HostRequest::Migration {
                environment: self.descriptor.environment.clone(),
                request: Box::new(request),
            },
        )
        .await?;
        ensure!(
            response.environment == self.descriptor.environment
                && response.status == OperationStatus::Verified,
            "migration helper action blocked: {}",
            response.error.unwrap_or_default()
        );
        response.migration.context("helper omitted migration proof")
    }
    fn grant(
        &self,
        plan: Plan,
        role: Role,
        actor: &str,
        ttl: u64,
        nonce: String,
        source_grant_sha256: Option<String>,
    ) -> Result<Signed<Grant>> {
        ensure!(
            (60..=MAX_GRANT_SECONDS).contains(&ttl),
            "migration approval must expire in 60 seconds to 24 hours"
        );
        let issued_at = now();
        Signed::sign(
            Grant {
                schema: 1,
                issuer: plan.peer(role).clone(),
                recipient: plan.other(role).clone(),
                plan,
                role,
                actor: actor.into(),
                nonce,
                source_grant_sha256,
                issued_at,
                expires_at: issued_at.checked_add(ttl).context("expiry overflow")?,
            },
            &self.identity,
            "grant",
        )
    }
    pub async fn handle_admin(&self, peer: &str, request: AdminRequest) -> Result<Value> {
        let actor = self.actor(peer)?;
        if !matches!(
            &request,
            AdminRequest::Describe {} | AdminRequest::List {} | AdminRequest::Status { .. }
        ) {
            ensure!(
                self.store.read()?.binding.feature("migration"),
                "root-configured physical scope does not authorize migration"
            );
        }
        match request {
            AdminRequest::Describe {} => Ok(json!(self.describe())),
            AdminRequest::List {} => Ok(json!(
                self.store
                    .read()?
                    .migrations
                    .into_values()
                    .collect::<Vec<_>>()
            )),
            AdminRequest::Status { migration_id } => Ok(json!(self.record(&migration_id)?)),
            AdminRequest::Plan {
                request_id,
                vm_id,
                destination,
                target_owner,
                target_vm_id,
                expires_in_seconds,
            } => {
                ensure!(
                    !request_id.is_empty()
                        && request_id.len() <= 96
                        && request_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                    "invalid migration request_id"
                );
                let migration_id = digest(&(self.public_peer(), peer, &request_id));
                if let Some(existing) = self.store.read()?.migrations.get(&migration_id) {
                    ensure!(
                        existing.actor == peer
                            && existing.plan.source_vm_id == vm_id
                            && existing.plan.target_vm_id == target_vm_id
                            && existing.plan.target_owner == target_owner
                            && existing.plan.destination.server_id
                                == destination.endpoint.id.to_string()
                            && existing.plan.destination.environment == destination.environment
                            && existing.source_grant.value.expires_at
                                - existing.source_grant.value.issued_at
                                == expires_in_seconds,
                        "migration request_id already belongs to another plan"
                    );
                    return Ok(json!(existing.source_grant));
                }
                let snapshot: Snapshot = serde_json::from_value(
                    self.helper(HostMigrationRequest::InspectVm { vm_id })
                        .await?,
                )?;
                ensure!(
                    snapshot.vm.state == VmState::Stopped,
                    "migration requires a stopped source guest"
                );
                let plan = Plan {
                    schema: 1,
                    migration_id: migration_id.clone(),
                    source: self.public_peer(),
                    destination: Peer {
                        server_id: destination.endpoint.id.to_string(),
                        environment: destination.environment.clone(),
                    },
                    source_vm_id: vm_id,
                    target_vm_id,
                    source_owner: snapshot.vm.owner.clone(),
                    target_owner,
                    name: snapshot.vm.name.clone(),
                    disk_mib: snapshot.vm.disk_mib,
                    memory_mib: snapshot.vm.memory_mib,
                    min_memory_mib: snapshot.vm.min_memory_mib,
                    cores: snapshot.vm.cores,
                    guest: snapshot.vm.guest.clone(),
                    source_machine_sha256: snapshot.machine_sha256,
                    source_config_sha256: snapshot.config_sha256,
                    byte_ceiling: snapshot.byte_ceiling,
                    target_bridge: "vmbr77".into(),
                };
                plan.validate()?;
                let grant = self.grant(
                    plan.clone(),
                    Role::Source,
                    peer,
                    expires_in_seconds,
                    digest(&("initial-source", &migration_id)),
                    None,
                )?;
                let record =
                    TransferRecord::source(plan, grant.clone(), peer.into(), destination, now())?;
                self.store.change(|c| {
                    admit_source(c, &record, &actor)
                        .map_err(|e| failure("conflict", &e.to_string()))?;
                    c.schema = 6;
                    c.migrations.insert(migration_id, record);
                    Ok(())
                })?;
                Ok(json!(grant))
            }
            AdminRequest::AuthorizeDestination {
                source_grant,
                expires_in_seconds,
            } => {
                source_grant.validate_grant(now(), false)?;
                ensure!(
                    source_grant.value.role == Role::Source
                        && source_grant.value.plan.destination == self.public_peer(),
                    "source authorization addressed to another controller"
                );
                let id = source_grant.value.plan.migration_id.clone();
                if let Some(existing) = self.store.read()?.migrations.get(&id) {
                    let grant = existing
                        .destination_grant
                        .as_ref()
                        .context("destination approval missing")?;
                    ensure!(
                        existing.actor == peer
                            && existing.source_grant == source_grant
                            && grant.value.expires_at - grant.value.issued_at == expires_in_seconds,
                        "destination already authorized a different plan/actor/lifetime"
                    );
                    return Ok(json!(grant));
                }
                let grant = self.grant(
                    source_grant.value.plan.clone(),
                    Role::Destination,
                    peer,
                    expires_in_seconds,
                    digest(&("initial-destination", &id)),
                    Some(digest(&source_grant)),
                )?;
                let record =
                    TransferRecord::destination(source_grant, grant.clone(), peer.into(), now())?;
                self.store.change(|c| {
                    admit_destination(c, &record, &actor)
                        .map_err(|e| failure("conflict", &e.to_string()))?;
                    reserve_destination(c, &record, peer, &actor)
                        .map_err(|e| failure("conflict", &e.to_string()))?;
                    c.schema = 6;
                    c.migrations.insert(id, record);
                    Ok(())
                })?;
                Ok(json!(grant))
            }
            AdminRequest::Renew {
                migration_id,
                request_id,
                expires_in_seconds,
                source_grant,
            } => {
                ensure!(
                    !request_id.is_empty()
                        && request_id.len() <= 96
                        && request_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
                    "invalid renewal request ID"
                );
                let fingerprint = digest(&(peer, expires_in_seconds, &source_grant));
                let current = self.record(&migration_id)?;
                if let Some(old) = current.renewals.get(&request_id) {
                    ensure!(
                        old.fingerprint == fingerprint,
                        "renewal request_id already used for different parameters"
                    );
                    return Ok(json!(old.grant));
                }
                ensure!(
                    current.handoff.is_none()
                        && matches!(
                            current.phase,
                            Phase::Authorized | Phase::Fenced | Phase::Receiving | Phase::Ready
                        ),
                    "renewal requires a verified precommit transfer; ambiguous work must be inspected first"
                );
                let grant = self.grant(
                    current.plan.clone(),
                    current.role,
                    peer,
                    expires_in_seconds,
                    digest(&("renewal", &migration_id, &request_id, peer, current.role)),
                    source_grant.as_ref().map(digest),
                )?;
                self.change(&migration_id,|r| {
                    ensure!(r.handoff.is_none() && matches!(r.phase,Phase::Authorized|Phase::Fenced|Phase::Receiving|Phase::Ready),"ownership committed during renewal; approval unchanged");
                    if r.role==Role::Source {
                        ensure!(source_grant.is_none(),"source renewal does not accept another source authorization");
                        r.source_grant=grant.clone();r.destination_grant=None;r.ready=None;
                        r.phase=if r.manifest.is_some(){Phase::Fenced}else{Phase::Authorized};
                    } else {
                        let source=source_grant.context("destination renewal requires a fresh source authorization")?;
                        source.validate_grant(now(),false)?;
                        ensure!(source.value.role==Role::Source && source.value.plan==r.plan && source.value.nonce!=r.source_grant.value.nonce,"destination renewal requires a newly authorized source nonce for the same plan");
                        r.source_grant=source;r.destination_grant=Some(grant.clone());r.ready=None;
                        r.phase=if r.manifest.is_some(){Phase::Receiving}else{Phase::Authorized};
                    }
                    ensure!(r.renewals.len()<32,"migration renewal history is full");
                    r.renewals.insert(request_id,RenewalRecord{fingerprint,grant:grant.clone()});
                    r.actor=peer.into();r.updated_at=now();r.error=None;Ok(json!(grant))
                })
            }
            AdminRequest::Start {
                migration_id,
                destination_grant,
            } => self.change(&migration_id, |r| {
                ensure!(
                    r.role == Role::Source,
                    "start must be submitted to source controller"
                );
                if !matches!(r.phase, Phase::Authorized | Phase::Fenced) {
                    ensure!(
                        r.destination_grant.as_ref() == Some(&destination_grant),
                        "start retry differs from accepted authorization"
                    );
                    return Ok(json!(r));
                }
                r.set_destination_grant(destination_grant, now())?;
                Ok(json!(r))
            }),
            AdminRequest::Abort {
                migration_id,
                confirm_cleanup,
                source_abort,
            } => {
                ensure!(
                    confirm_cleanup,
                    "explicit staging cleanup confirmation required"
                );
                let r = self.record(&migration_id)?;
                if r.phase == Phase::Aborted {
                    return Ok(json!(r));
                }
                if r.role == Role::Destination {
                    r.validate_abort_proof(
                        source_abort
                            .as_ref()
                            .context("destination cleanup requires signed source cancellation")?,
                    )?;
                } else {
                    ensure!(
                        source_abort.is_none(),
                        "source abort does not consume another cancellation proof"
                    );
                }
                self.change(&migration_id, |r| {
                    ensure!(
                        r.handoff.is_none()
                            && !matches!(
                                r.phase,
                                Phase::Relinquished | Phase::Accepted | Phase::Complete
                            ),
                        "ownership already committed; reverse migration is required"
                    );
                    if r.role == Role::Destination {
                        r.abort_proof = source_abort;
                    }
                    r.phase = Phase::Cleaning;
                    r.updated_at = now();
                    r.error = None;
                    Ok(json!(r))
                })
            }
            AdminRequest::Finish {
                migration_id,
                confirm_guest_verified,
                confirm_delete_recovery,
            } => {
                ensure!(
                    confirm_guest_verified && confirm_delete_recovery,
                    "guest verification and recovery deletion must both be explicitly acknowledged"
                );
                let r = self.record(&migration_id)?;
                ensure!(
                    r.role == Role::Source,
                    "finish belongs to source controller"
                );
                if r.phase == Phase::Complete {
                    return Ok(json!(r));
                }
                r.handoff
                    .as_ref()
                    .context("source has not relinquished ownership")?;
                r.acceptance
                    .as_ref()
                    .context("destination has not acknowledged durable ownership")?;
                self.change(&migration_id, |r| {
                    r.phase = Phase::Cleaning;
                    r.updated_at = now();
                    Ok(())
                })?;
                Ok(json!(self.record(&migration_id)?))
            }
        }
    }

    async fn peer_request(&self, request: PeerRequest, remote: &str) -> Result<Value> {
        ensure!(
            request.schema == 1 && request.destination_environment == self.descriptor.environment,
            "migration destination identity mismatch"
        );
        let permanent = matches!(
            request.message,
            PeerMessage::Status {} | PeerMessage::Accept { .. }
        );
        request.grant.validate_grant(now(), permanent)?;
        ensure!(
            request.grant.value.role == Role::Destination
                && request.grant.value.issuer == self.public_peer()
                && request.grant.value.recipient.server_id == remote,
            "migration grant does not authorize this authenticated peer/recipient"
        );
        let id = request.grant.value.plan.migration_id.clone();
        let record = self.record(&id)?;
        ensure!(
            record.role == Role::Destination
                && record.destination_grant.as_ref() == Some(&request.grant),
            "grant was never durably authorized locally or was replaced"
        );
        match request.message {
            PeerMessage::Status {} => Ok(json!(record)),
            PeerMessage::Manifest { manifest } => {
                if record.phase == Phase::Blocked {
                    bail!("destination migration is blocked; inspect retained state");
                }
                if let Some(old) = &record.manifest {
                    ensure!(old == &manifest, "stopped manifest cannot be replaced");
                    return Ok(json!(record));
                }
                let catalog = self.store.read()?;
                let create = catalog
                    .operations
                    .get(&target_create_id(&id))
                    .context("destination allocation operation missing")?;
                ensure!(
                    create.public.status == OperationStatus::Verified,
                    "destination allocation is not verified ready"
                );
                manifest.verify(&record.plan.source.server_id, "manifest")?;
                manifest.value.validate(&record.plan)?;
                self.change(&id, |r| {
                    r.phase = Phase::Preparing;
                    r.updated_at = now();
                    Ok(())
                })?;
                let proof = self
                    .helper(HostMigrationRequest::Prepare {
                        plan: record.plan.clone(),
                        role: Role::Destination,
                        manifest: Some(manifest.value.clone()),
                    })
                    .await?;
                self.change(&id, |r| {
                    r.helper_proof = Some(proof);
                    r.set_manifest(manifest, now())?;
                    Ok(json!(r))
                })
            }
            PeerMessage::Chunk {
                slot,
                offset,
                bytes_base64,
                sha256,
                zero_bytes,
            } => {
                ensure!(
                    record.phase == Phase::Receiving,
                    "destination is not receiving this transfer"
                );
                let proof = self
                    .helper(HostMigrationRequest::WriteChunk {
                        migration_id: id.clone(),
                        slot: slot.clone(),
                        offset,
                        bytes_base64,
                        sha256,
                        zero_bytes,
                    })
                    .await?;
                let next = proof["next_offset"]
                    .as_u64()
                    .context("helper omitted chunk offset")?;
                self.change(&id, |r| {
                    let previous = r.offsets.get(&slot).copied().unwrap_or(0);
                    ensure!(next >= previous, "helper receive progress regressed");
                    r.offsets.insert(slot, next);
                    r.updated_at = now();
                    Ok(json!({"next_offset":next}))
                })
            }
            PeerMessage::Seal {} => {
                if let Some(ready) = record.ready {
                    return Ok(json!(ready));
                }
                ensure!(
                    record.phase == Phase::Receiving,
                    "destination is not receiving"
                );
                self.change(&id, |r| {
                    ensure!(r.phase == Phase::Receiving, "target receive phase changed");
                    r.seal_requested = true;
                    r.updated_at = now();
                    Ok(json!({"pending":true}))
                })
            }

            PeerMessage::Accept { handoff } => {
                if let Some(existing) = &record.handoff {
                    ensure!(
                        existing == &handoff,
                        "handoff retry differs from committed ownership"
                    );
                }
                if record.phase == Phase::Complete {
                    return Ok(json!(
                        record
                            .acceptance
                            .context("accepted transfer omitted receipt")?
                    ));
                }
                self.change(&id, |r| {
                    r.accept(handoff.clone(), now())?;
                    Ok(())
                })?;
                // Persist ownership acceptance BEFORE unlocking a target disk.
                self.helper(HostMigrationRequest::Activate {
                    migration_id: id.clone(),
                    handoff: handoff.clone(),
                })
                .await?;
                let acceptance = Signed::sign(
                    Acceptance {
                        migration_id: id.clone(),
                        plan_sha256: digest(&record.plan),
                        handoff_sha256: digest(&handoff.value),
                        destination: self.public_peer(),
                        accepted_at: now(),
                    },
                    &self.identity,
                    "accepted",
                )?;
                self.store
                    .change(|c| {
                        let r = c
                            .migrations
                            .get_mut(&id)
                            .ok_or_else(|| failure("invalid", "migration missing"))?;
                        r.acknowledge(acceptance.clone(), now())
                            .map_err(|e| failure("conflict", &e.to_string()))?;
                        r.phase = Phase::Complete;
                        r.updated_at = now();
                        c.vms
                            .get_mut(&r.plan.target_vm_id)
                            .ok_or_else(|| failure("internal", "incoming VM missing"))?
                            .state = VmState::Stopped;
                        Ok(json!(acceptance))
                    })
                    .map_err(Into::into)
            }
        }
    }

    async fn call(&self, record: &TransferRecord, message: PeerMessage) -> Result<Value> {
        let grant = record
            .destination_grant
            .clone()
            .context("destination approval missing")?;
        let route = record
            .peer_route
            .clone()
            .context("destination route missing")?;
        ensure!(
            route.endpoint.id.to_string() == record.plan.destination.server_id
                && route.environment == record.plan.destination.environment,
            "destination route no longer matches authority"
        );
        let peer = record.plan.destination.server_id.clone();
        let cached = self
            .connections
            .lock()
            .await
            .get(&peer)
            .filter(|c| c.close_reason().is_none())
            .cloned();
        let connection = if let Some(connection) = cached {
            connection
        } else {
            let connection = timeout(
                Duration::from_secs(30),
                self.endpoint.connect(route.endpoint, ALPN),
            )
            .await
            .context("migration peer connection timed out")??;
            ensure!(
                connection.remote_id().to_string() == peer,
                "connected migration identity differs from plan"
            );
            self.connections
                .lock()
                .await
                .insert(peer, connection.clone());
            connection
        };
        let (mut send, mut receive) = connection.open_bi().await?;
        let data = serde_json::to_vec(&PeerRequest {
            schema: 1,
            destination_environment: record.plan.destination.environment.clone(),
            grant,
            message,
        })?;
        ensure!(
            data.len() <= MAX_FRAME,
            "migration peer frame exceeds bound"
        );
        send.write_all(&data).await?;
        send.finish()?;
        let bytes = timeout(Duration::from_secs(120), receive.read_to_end(MAX_FRAME))
            .await
            .context("migration peer reply timed out; exact request may be retried")??;
        let response: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            response["ok"] == true,
            "migration peer refused request: {}",
            response["error"].as_str().unwrap_or("invalid reply")
        );

        Ok(response["result"].clone())
    }
    async fn seal(&self, id: &str) -> Result<()> {
        let record = self.record(id)?;
        ensure!(
            record.role == Role::Destination
                && record.phase == Phase::Receiving
                && record.seal_requested,
            "target seal intent changed"
        );
        record.fresh(now())?;
        let manifest = record
            .manifest
            .as_ref()
            .context("stopped manifest missing")?
            .value
            .clone();
        for (slot, v) in manifest.volumes.iter().chain(manifest.snippets.iter()) {
            ensure!(
                record.offsets.get(slot) == Some(&v.bytes),
                "target volume is incomplete"
            );
        }
        let target_identities = self
            .helper(HostMigrationRequest::Seal {
                migration_id: id.into(),
                manifest,
            })
            .await?;
        let ready = Signed::sign(
            ReadyReceipt {
                migration_id: id.into(),
                plan_sha256: digest(&record.plan),
                manifest_sha256: digest(
                    &record.manifest.as_ref().context("manifest missing")?.value,
                ),
                destination: self.public_peer(),
                target_identities,
                destination_grant_sha256: digest(
                    record
                        .destination_grant
                        .as_ref()
                        .context("destination approval missing")?,
                ),
            },
            &self.identity,
            "ready",
        )?;
        self.change(id, |r| {
            r.set_ready(ready, now())?;
            r.seal_requested = false;
            Ok(())
        })
    }
    async fn cleanup(&self, id: &str) -> Result<()> {
        let record = self.record(id)?;
        ensure!(record.phase == Phase::Cleaning, "cleanup intent changed");
        if let Some(handoff) = record.handoff.clone() {
            ensure!(
                record.role == Role::Source,
                "destination cannot delete accepted ownership as cleanup"
            );
            let acceptance = record
                .acceptance
                .clone()
                .context("destination acceptance missing")?;
            self.helper(HostMigrationRequest::Cleanup {
                migration_id: id.into(),
                confirm: true,
                handoff,
                acceptance,
            })
            .await?;
            self.store.change(|c| {
                let r = c
                    .migrations
                    .get_mut(id)
                    .ok_or_else(|| failure("invalid", "migration missing"))?;
                r.phase = Phase::Complete;
                r.updated_at = now();
                r.error = None;
                c.vms
                    .get_mut(&r.plan.source_vm_id)
                    .ok_or_else(|| failure("internal", "source VM missing"))?
                    .state = VmState::Deleted;
                Ok(())
            })?;
        } else {
            self.helper(HostMigrationRequest::Abort {
                migration_id: id.into(),
                confirm_cleanup: true,
                source_abort: record.abort_proof.clone(),
                plan: Some(record.plan.clone()),
            })
            .await?;
            self.store.change(|c| {
                let r = c
                    .migrations
                    .get_mut(id)
                    .ok_or_else(|| failure("invalid", "migration missing"))?;
                if r.role == Role::Source {
                    r.abort_proof = Some(
                        Signed::sign(
                            AbortReceipt {
                                migration_id: id.into(),
                                plan_sha256: digest(&r.plan),
                                source: r.plan.source.clone(),
                                aborted_at: now(),
                            },
                            &self.identity,
                            "abort",
                        )
                        .map_err(|e| failure("internal", &e.to_string()))?,
                    );
                }
                r.abort(now())
                    .map_err(|e| failure("conflict", &e.to_string()))?;
                r.error = None;
                if r.role == Role::Destination {
                    c.vms
                        .get_mut(&r.plan.target_vm_id)
                        .ok_or_else(|| failure("internal", "staged VM missing"))?
                        .state = VmState::Deleted;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
    async fn coordinate(&self, id: &str) -> Result<()> {
        let mut record = self.record(id)?;
        if record.phase == Phase::Cleaning {
            return self.cleanup(id).await;
        }
        if record.role == Role::Destination {
            return self.seal(id).await;
        }
        if record.phase == Phase::Authorized {
            if record.destination_grant.is_none() {
                return Ok(());
            }
            record.fresh(now())?;
            self.change(id, |r| {
                ensure!(
                    r.phase == Phase::Authorized
                        && r.destination_grant.is_some()
                        && r.handoff.is_none(),
                    "source preparation canceled or changed"
                );
                r.phase = Phase::Preparing;
                r.updated_at = now();
                Ok(())
            })?;
            self.helper(HostMigrationRequest::Prepare {
                plan: record.plan.clone(),
                role: Role::Source,
                manifest: None,
            })
            .await?;
            ensure!(
                self.record(id)?.phase == Phase::Preparing,
                "source preparation was canceled"
            );
            let manifest: Manifest = serde_json::from_value(
                self.helper(HostMigrationRequest::Fence {
                    migration_id: id.into(),
                })
                .await?,
            )?;
            let manifest = Signed::sign(manifest, &self.identity, "manifest")?;
            self.change(id, |r| r.set_manifest(manifest, now()))?;
            record = self.record(id)?;
        }
        if record.phase == Phase::Preparing {
            let proof = self
                .helper(HostMigrationRequest::Inspect {
                    migration_id: id.into(),
                })
                .await?;
            let manifest: Manifest = if proof["phase"] == "authorized" {
                serde_json::from_value(
                    self.helper(HostMigrationRequest::Fence {
                        migration_id: id.into(),
                    })
                    .await?,
                )?
            } else {
                ensure!(
                    proof["phase"] == "fenced",
                    "interrupted source fence lacks independently verified completion; inspect retained original"
                );
                serde_json::from_value(proof["manifest"].clone())?
            };
            self.change(id, |r| {
                r.set_manifest(Signed::sign(manifest, &self.identity, "manifest")?, now())
            })?;
            record = self.record(id)?;
        }
        if record.phase == Phase::Fenced {
            record.fresh(now())?;
            let manifest = record.manifest.clone().context("source manifest missing")?;
            let target: TransferRecord = serde_json::from_value(
                self.call(
                    &record,
                    PeerMessage::Manifest {
                        manifest: manifest.clone(),
                    },
                )
                .await?,
            )?;
            ensure!(
                target.plan == record.plan && target.role == Role::Destination,
                "destination replied for another transfer"
            );
            for (slot, volume) in manifest
                .value
                .volumes
                .iter()
                .chain(manifest.value.snippets.iter())
            {
                let mut offset = target.offsets.get(slot).copied().unwrap_or(0);
                ensure!(
                    offset <= volume.bytes,
                    "destination offset exceeds manifest"
                );
                while offset < volume.bytes {
                    record = self.record(id)?;
                    ensure!(
                        record.phase == Phase::Fenced,
                        "source transfer was canceled or changed during copy"
                    );
                    record.fresh(now())?;
                    let chunk = self
                        .helper(HostMigrationRequest::ReadChunk {
                            migration_id: id.into(),
                            slot: slot.clone(),
                            offset,
                        })
                        .await?;
                    let next = self
                        .call(
                            &record,
                            PeerMessage::Chunk {
                                slot: slot.clone(),
                                offset,
                                bytes_base64: chunk["bytes_base64"]
                                    .as_str()
                                    .context("helper omitted chunk bytes")?
                                    .into(),
                                sha256: chunk["sha256"]
                                    .as_str()
                                    .context("helper omitted chunk hash")?
                                    .into(),
                                zero_bytes: chunk["zero_bytes"].as_u64(),
                            },
                        )
                        .await?["next_offset"]
                        .as_u64()
                        .context("peer omitted next offset")?;
                    ensure!(
                        next > offset && next <= volume.bytes && next - offset <= 8 * 1024 * 1024,
                        "invalid receive progress"
                    );
                    offset = next;
                    self.change(id, |r| {
                        r.offsets.insert(slot.clone(), offset);
                        r.updated_at = now();
                        Ok(())
                    })?;
                }
            }
            let reply = self.call(&record, PeerMessage::Seal {}).await?;
            if reply["pending"] == true {
                return Ok(());
            }
            let ready: Signed<ReadyReceipt> = serde_json::from_value(reply)?;
            self.change(id, |r| r.set_ready(ready, now()))?;
            record = self.record(id)?;
        }
        if record.phase == Phase::Ready {
            let inspection = self
                .helper(HostMigrationRequest::Inspect {
                    migration_id: id.into(),
                })
                .await?;
            ensure!(
                inspection["inspection"]["fence_verified"] == true,
                "source fence/stopped identity not verified before ownership commit"
            );
            let handoff = self.change(id, |r| r.relinquish(&self.identity, now()))?;
            self.helper(HostMigrationRequest::Commit {
                migration_id: id.into(),
                handoff,
            })
            .await?;
            record = self.record(id)?;
        }
        if record.phase == Phase::Relinquished {
            let handoff = record
                .handoff
                .clone()
                .context("committed handoff missing")?;
            // Repeat a lost acknowledgement with this exact durable proof.
            self.helper(HostMigrationRequest::Commit {
                migration_id: id.into(),
                handoff: handoff.clone(),
            })
            .await?;
            let acceptance: Signed<Acceptance> =
                serde_json::from_value(self.call(&record, PeerMessage::Accept { handoff }).await?)?;
            self.change(id, |r| r.acknowledge(acceptance, now()))?;
        }
        Ok(())
    }
    pub async fn work_once(&self) -> Result<()> {
        let Ok(_permit) = self.coordinator.try_acquire() else {
            return Ok(());
        };
        let ids: Vec<_> = self
            .store
            .read()?
            .migrations
            .into_iter()
            .filter(|(_, r)| {
                r.phase == Phase::Cleaning
                    || r.role == Role::Destination
                        && r.phase == Phase::Receiving
                        && r.seal_requested
                    || r.role == Role::Source
                        && matches!(
                            r.phase,
                            Phase::Authorized
                                | Phase::Preparing
                                | Phase::Fenced
                                | Phase::Ready
                                | Phase::Relinquished
                        )
                        && r.acceptance.is_none()
            })
            .map(|(id, _)| id)
            .collect();
        for id in ids {
            match self.coordinate(&id).await {
                Err(error) => {
                    self.change(&id, |r| {
                        if !r.phase.terminal() {
                            r.error = Some(error.to_string());
                            r.updated_at = now();
                            if error.to_string().contains("ambiguous") && r.phase != Phase::Cleaning
                            {
                                r.phase = Phase::Blocked;
                            }
                        }
                        Ok(())
                    })?;
                }
                Ok(()) => {
                    self.change(&id, |r| {
                        r.error = None;
                        Ok(())
                    })?;
                }
            }
        }
        Ok(())
    }
    async fn connection(&self, incoming: iroh::endpoint::Incoming) -> Result<()> {
        let connection = timeout(Duration::from_secs(20), incoming)
            .await
            .context("migration handshake deadline")??;
        let peer = connection.remote_id().to_string();
        loop {
            let (mut send, mut receive) =
                match timeout(Duration::from_secs(300), connection.accept_bi()).await {
                    Ok(Ok(stream)) => stream,
                    _ => return Ok(()),
                };
            let result = match timeout(Duration::from_secs(10), receive.read_to_end(MAX_FRAME))
                .await
                .context("migration request read deadline")?
            {
                Ok(bytes) => match serde_json::from_slice::<PeerRequest>(&bytes) {
                    Ok(request) => self.peer_request(request, &peer).await,
                    Err(_) => Err(anyhow::anyhow!("malformed migration peer request")),
                },
                Err(error) => Err(error.into()),
            };
            let response = match result {
                Ok(result) => json!({"ok":true,"result":result}),
                Err(error) => json!({"ok":false,"error":error.to_string()}),
            };
            let bytes = serde_json::to_vec(&response)?;
            ensure!(bytes.len() <= MAX_FRAME, "migration response exceeds bound");
            send.write_all(&bytes).await?;
            send.finish()?;
            if response["ok"] != true {
                connection.close(1u32.into(), b"migration request refused");
                return Ok(());
            }
        }
    }
    pub async fn serve(self: Arc<Self>, shutdown: impl Future<Output = ()>) -> Result<()> {
        let mut tasks = JoinSet::new();
        let permits = Arc::new(Semaphore::new(16));
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _=&mut shutdown=>break,
                _=interval.tick()=>{if self.coordinator.available_permits()>0 {let service=self.clone();tasks.spawn(async move{service.work_once().await});}},
                completed=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Ok(Err(error)))=completed {eprintln!("migration service: {error:#}");}},
                incoming=self.endpoint.accept()=>{let Some(incoming)=incoming else{break;};let Ok(permit)=permits.clone().try_acquire_owned() else{incoming.refuse();continue;};let service=self.clone();tasks.spawn(async move{let _permit=permit;timeout(Duration::from_secs(3600),service.connection(incoming)).await.context("migration connection deadline")?});}
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.endpoint.close().await;
        Ok(())
    }
}

pub fn target_create_id(id: &str) -> String {
    digest(&("migration-target-create", id))
}
fn busy(c: &Catalog, owner: &str) -> bool {
    c.operations
        .values()
        .any(|r| r.public.owner == owner && !r.public.status.is_terminal())
        || c.disk_shrinks.values().any(|r| r.owner == owner)
        || c.migrations.values().any(|r| {
            !r.phase.terminal()
                && (if r.role == Role::Source {
                    &r.plan.source_owner
                } else {
                    &r.plan.target_owner
                }) == owner
        })
}
fn admit_source(c: &Catalog, r: &TransferRecord, actor: &User) -> Result<()> {
    ensure!(
        actor.role == UserRole::Admin && r.plan.source.environment == c.binding.environment,
        "source admin/environment mismatch"
    );
    let vm = c
        .vms
        .get(&r.plan.source_vm_id)
        .context("source VM missing")?;
    let expected = r.plan.vm(Role::Source);
    ensure!(
        serde_json::to_value(vm)? == serde_json::to_value(expected)? && !busy(c, &vm.owner),
        "source profile changed or owner has unsettled work"
    );
    let owner = c.users.get(&vm.owner).context("source owner missing")?;
    ensure!(
        owner.ready && owner.state == AccountState::Active,
        "source owner inactive"
    );
    Ok(())
}
fn admit_destination(c: &Catalog, r: &TransferRecord, actor: &User) -> Result<()> {
    ensure!(
        actor.role == UserRole::Admin && r.plan.destination.environment == c.binding.environment,
        "destination admin/environment mismatch"
    );
    ensure!(
        !c.vms.contains_key(&r.plan.target_vm_id),
        "destination VMID occupied or previously used"
    );
    let owner = c
        .users
        .get(&r.plan.target_owner)
        .context("destination owner missing")?;
    ensure!(
        owner.ready && owner.state == AccountState::Active && !busy(c, &owner.user_id),
        "destination owner inactive or busy"
    );
    let existing: Vec<_> = c
        .vms
        .values()
        .filter(|v| v.owner == owner.user_id && v.state != VmState::Deleted)
        .collect();
    let vm = r.plan.vm(Role::Destination);
    let limits = c.binding.limits();
    ensure!(
        vm.disk_mib <= limits.disk_mib
            && vm.memory_mib <= limits.memory_mib
            && vm.min_memory_mib <= limits.min_memory_mib
            && vm.cores <= limits.cores,
        "incoming VM exceeds root-configured host limits"
    );
    ensure!(
        existing
            .iter()
            .map(|v| super::store::disk_charge(v))
            .sum::<u64>()
            + super::store::disk_charge(&vm)
            <= owner.budget_mib
            && existing.len() < owner.max_vms as usize
            && existing.iter().map(|v| v.min_memory_mib).sum::<u64>() + vm.min_memory_mib
                <= owner.min_memory_budget_mib
            && existing.iter().map(|v| v.memory_mib).sum::<u64>() + vm.memory_mib
                <= owner.max_memory_budget_mib
            && existing.iter().map(|v| u64::from(v.cores)).sum::<u64>() + u64::from(vm.cores)
                <= u64::from(owner.max_cores),
        "destination policy cannot reserve incoming VM"
    );
    if let Some(scope) = &c.binding.physical_trial {
        ensure!(
            scope.scope.permits(&owner.user_id, Some(vm.vm_id)),
            "incoming VM escapes physical test scope"
        );
    }
    Ok(())
}
fn reserve_destination(
    c: &mut Catalog,
    r: &TransferRecord,
    peer: &str,
    actor: &User,
) -> Result<()> {
    let mut vm = r.plan.vm(Role::Destination);
    vm.state = VmState::Creating;
    let owner = c
        .users
        .get(&vm.owner)
        .context("destination owner missing")?
        .clone();
    // Allocate an empty destination. Re-importing the base image is both
    // unnecessary and incorrect for a source that has since been shrunk.
    let id = target_create_id(&r.plan.migration_id);
    let kind = "create";
    let mut staging = vm.clone();
    staging.guest = None;
    let job: Job = serde_json::from_value(
        json!({"operation_id":id,"kind":kind,"owner":owner,"vm":staging,"extension":{"kind":"incoming_migration","spec":r.plan}}),
    )?;
    let public = serde_json::from_value(
        json!({"operation_id":id,"owner":vm.owner,"request_id":format!("migration-target-{}",&r.plan.migration_id[..32]),"kind":kind,"vm_id":vm.vm_id,"status":"queued","step":"queued","error":null,"created_at":now()}),
    )?;
    let request_id = format!("migration-target-{}", &r.plan.migration_id[..32]);
    let request: super::protocol::Request = serde_json::from_value(
        json!({"action":"create_vm","request_id":request_id,"name":vm.name,"disk_mib":vm.disk_mib,"memory_mib":vm.memory_mib,"min_memory_mib":vm.min_memory_mib,"cores":vm.cores,"owner":vm.owner,"guest":null}),
    )?;
    let request_key = format!("{}/{}", actor.user_id, request_id);
    ensure!(
        !c.requests.contains_key(&request_key),
        "reserved migration create request ID already used"
    );
    let record = Record {
        public,
        actor: peer.into(),
        fingerprint: super::store::digest(&request),
        job,
    };
    c.requests.insert(request_key, id.clone());
    c.audit.push(json!({"at":now(),"actor":actor.user_id,"action":"migration_destination_reserved","migration_id":r.plan.migration_id,"operation_id":id,"vm_id":vm.vm_id}));
    c.operations.insert(id, record);
    c.vms.insert(vm.vm_id, vm);
    Ok(())
}
