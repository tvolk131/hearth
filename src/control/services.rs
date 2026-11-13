//! Live bound helper routing. Pure catalog requests remain usable in simulation;
//! read-only observations and queued mutations never bypass root-side scope.
use super::{
    host::{self, HostRequest},
    migration_transport,
    protocol::*,
    store::{Store, failure},
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

pub struct Services {
    pub host_socket: PathBuf,
    pub migration: Option<Arc<migration_transport::Service>>,
}
impl Services {
    async fn helper(
        &self,
        store: &Store,
        request: HostRequest,
    ) -> Result<host::HostResponse, ApiError> {
        let response = host::rpc(&self.host_socket, &request)
            .await
            .map_err(|e| failure("unavailable", &format!("{e:#}")))?;
        let binding = store
            .read()
            .map_err(|e| failure("internal", &e.to_string()))?
            .binding;
        if !response
            .binding
            .as_ref()
            .is_some_and(|b| binding.attested_by(b))
        {
            return Err(failure("forbidden", "helper backend/scope binding changed"));
        }
        Ok(response)
    }
    fn administrator(store: &Store, peer: &str) -> Result<User, ApiError> {
        let actor = store.actor(peer)?;
        if actor.role != Role::Admin || !actor.ready {
            return Err(failure("forbidden", "ready administrator required"));
        }
        Ok(actor)
    }
    pub async fn handle(
        &self,
        store: &Store,
        peer: &str,
        request: Request,
    ) -> Result<Value, ApiError> {
        match request {
            Request::Dashboard {} => {
                let mut dashboard = store.handle(peer, Request::Dashboard {})?;
                let catalog = store
                    .read()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                if let Some(managed) = &catalog.binding.managed {
                    let response = self
                        .helper(
                            store,
                            HostRequest::VmStates {
                                binding: Box::new(host::HostBinding {
                                    environment: catalog.binding.environment.clone(),
                                    backend: catalog.binding.backend.clone(),
                                    pool_guid: managed.pool_guid.clone(),
                                    managed: Some(managed.clone()),
                                    physical_trial: None,
                                }),
                            },
                        )
                        .await?;
                    let states = response
                        .inspection
                        .and_then(|v| v.as_array().cloned())
                        .ok_or_else(|| failure("internal", "invalid runtime inventory"))?;
                    if let Some(vms) = dashboard["vms"].as_array_mut() {
                        for vm in vms {
                            let id = vm["vm_id"].as_u64();
                            if matches!(vm["state"].as_str(), Some("running" | "stopped"))
                                && !catalog.operations.values().any(|o| {
                                    o.public.vm_id.map(u64::from) == id
                                        && !o.public.status.is_terminal()
                                })
                                && let Some(observed) =
                                    states.iter().find(|s| s["vmid"].as_u64() == id)
                                && matches!(
                                    observed["status"].as_str(),
                                    Some("running" | "stopped")
                                )
                            {
                                vm["state"] = observed["status"].clone();
                            }
                        }
                    }
                }
                Ok(dashboard)
            }
            Request::HostInventory {} => {
                Self::administrator(store, peer)?;
                let catalog = store
                    .read()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                let binding = super::host::HostBinding {
                    environment: catalog.binding.environment.clone(),
                    backend: catalog.binding.backend.clone(),
                    pool_guid: catalog
                        .binding
                        .managed
                        .as_ref()
                        .ok_or_else(|| {
                            failure("forbidden", "host inventory requires a managed backend")
                        })?
                        .pool_guid
                        .clone(),
                    managed: catalog.binding.managed.clone(),
                    physical_trial: None,
                };
                let response = self
                    .helper(
                        store,
                        HostRequest::Inventory {
                            binding: Box::new(binding),
                        },
                    )
                    .await?;
                let mut inventory = response
                    .inspection
                    .ok_or_else(|| failure("internal", "host inventory missing"))?;
                inventory["storage_allocations"] = json!(super::adoption::allocations(&catalog));
                Ok(inventory)
            }
            Request::InspectOperation { operation_id } => {
                Self::administrator(store, peer)?;
                let c = store
                    .read()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                if !c.binding.feature("recovery") {
                    return Err(failure(
                        "forbidden",
                        "scope does not authorize recovery inspection",
                    ));
                }
                let operation = c
                    .operations
                    .get(&operation_id)
                    .ok_or_else(|| failure("invalid", "operation not found"))?
                    .public
                    .clone();
                let response = self
                    .helper(
                        store,
                        HostRequest::InspectOperation {
                            environment: c.binding.environment,
                            operation_id,
                        },
                    )
                    .await?;
                let mut inspection = response
                    .inspection
                    .ok_or_else(|| failure("internal", "helper inspection missing"))?;
                inspection["operation"] = json!(operation);
                Ok(inspection)
            }
            Request::RecoverOperation {
                request_id,
                operation_id,
                inspection_id,
                kind,
                confirm_recovery,
            } => {
                Self::administrator(store, peer)?;
                if let Some(result) = store.recovery_retry(
                    peer,
                    &request_id,
                    &operation_id,
                    &inspection_id,
                    &kind,
                    confirm_recovery,
                )? {
                    return Ok(result);
                }
                let c = store
                    .read()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                if !c.binding.feature("recovery") {
                    return Err(failure("forbidden", "scope does not authorize recovery"));
                }
                let response = self
                    .helper(
                        store,
                        HostRequest::InspectOperation {
                            environment: c.binding.environment,
                            operation_id: operation_id.clone(),
                        },
                    )
                    .await?;
                let review = response
                    .inspection
                    .ok_or_else(|| failure("internal", "helper inspection missing"))?;
                let actions: Vec<super::vm_change::RecoveryKind> =
                    serde_json::from_value(review["allowed_actions"].clone())
                        .map_err(|_| failure("internal", "invalid helper action list"))?;
                if review["inspection_id"] != inspection_id || !actions.contains(&kind) {
                    return Err(failure(
                        "conflict",
                        "inspection is stale or does not authorize this action; inspect again",
                    ));
                }
                store.queue_reviewed_recovery(
                    peer,
                    &request_id,
                    &operation_id,
                    &inspection_id,
                    kind,
                    confirm_recovery,
                )
            }
            Request::PlanVmEnrollment { vm_id, owner } => {
                Self::administrator(store, peer)?;
                let c = store
                    .read()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                if !c.binding.feature("enrollment") {
                    return Err(failure("forbidden", "scope does not authorize enrollment"));
                }
                let response = self
                    .helper(
                        store,
                        HostRequest::PlanEnrollment {
                            environment: c.binding.environment,
                            vm_id,
                            owner,
                        },
                    )
                    .await?;
                let plan: super::vm_change::EnrollmentPlan = serde_json::from_value(
                    response
                        .enrollment
                        .ok_or_else(|| failure("internal", "helper enrollment plan missing"))?,
                )
                .map_err(|_| failure("internal", "invalid helper enrollment plan"))?;
                store.remember_enrollment_plan(peer, plan.clone())?;
                Ok(json!(plan))
            }
            Request::Migration { request } => {
                Self::administrator(store, peer)?;
                let c = store
                    .read()
                    .map_err(|e| failure("internal", &e.to_string()))?;
                if !c.binding.feature("migration") {
                    return Err(failure("forbidden", "scope does not authorize migration"));
                }
                self.migration
                    .as_ref()
                    .ok_or_else(|| failure("unavailable", "migration service is disabled"))?
                    .handle_admin(peer, *request)
                    .await
                    .map_err(|e| failure("conflict", &format!("{e:#}")))
            }
            other => store.handle(peer, other),
        }
    }
}
