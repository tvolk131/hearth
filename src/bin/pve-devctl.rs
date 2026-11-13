use std::{
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use pve_dev::control::{
    migration::{
        AbortReceipt, AdminRequest as MigrationRequest, Grant, MAX_GRANT_SECONDS,
        Role as MigrationRole, Signed,
    },
    protocol::{Guest, MAX_REQUEST, MAX_RESPONSE, Request, ResourcePolicy, VmAction},
    transport::{self, Descriptor, Profile, ProfileBinding},
    vm_change::{EnrollmentPlan, OperationInspection, RecoveryKind},
};
use serde::de::DeserializeOwned;

#[derive(Parser)]
#[command(about = "Authenticated client for the experimental pve-dev control daemon")]
struct Cli {
    #[arg(long, global = true)]
    profile: Option<PathBuf>,
    #[arg(long, global = true, requires_all = ["expected_environment", "expected_device_id"])]
    expected_server_id: Option<String>,
    #[arg(long, global = true, requires_all = ["expected_server_id", "expected_device_id"])]
    expected_environment: Option<String>,
    #[arg(long, global = true, requires_all = ["expected_server_id", "expected_environment"])]
    expected_device_id: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    Profile {
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        server: PathBuf,
        #[arg(long)]
        expected_server: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        direct_address: Option<SocketAddr>,
    },
    Rpc,
    /// Inspect the public identities in a local profile without connecting.
    ProfileInfo,
    Dashboard,
    Whoami,
    Budget,
    Images,
    /// Administrator-approved bootable image catalog with login defaults.
    ImageCatalog,
    CreateUser {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        budget_mib: u64,
        #[arg(long, default_value = "256")]
        min_memory_budget_mib: u64,
        #[arg(long, default_value = "1024")]
        max_memory_budget_mib: u64,
        #[arg(long, default_value = "8")]
        max_cores: u16,
        #[arg(long, default_value = "4")]
        max_vms: u16,
    },
    UpdateUserPolicy {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long)]
        budget_mib: u64,
        #[arg(long)]
        min_memory_budget_mib: u64,
        #[arg(long)]
        max_memory_budget_mib: u64,
        #[arg(long)]
        max_cores: u16,
        #[arg(long)]
        max_vms: u16,
    },
    DeleteUser {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        expected_revision: u64,
    },
    EnrollDevice {
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        endpoint_id: String,
        #[arg(long)]
        label: String,
    },
    RevokeDevice {
        #[arg(long)]
        endpoint_id: String,
    },
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
    Operations {
        #[arg(long)]
        operation_id: Option<String>,
        #[command(subcommand)]
        command: Option<OperationsCommand>,
    },
    /// Review, authorize and track an independently controlled stopped transfer.
    Migration {
        #[command(subcommand)]
        command: MigrationCommand,
    },
}

#[derive(Subcommand)]
enum IdentityCommand {
    New {
        #[arg(long)]
        path: PathBuf,
    },
    Show {
        #[arg(long)]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum VmCommand {
    List,
    /// Estimate a lab shrink's admission/workspace requirements (no disk writes).
    ShrinkPlan {
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        target_disk_mib: u64,
        #[arg(long, value_parser = partition_uuid)]
        root_partition_uuid: Option<String>,
    },
    /// Prepare a smaller disk for a stopped ext4/GPT lab guest; retain recovery.
    Shrink {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        expected_disk_mib: u64,
        #[arg(long)]
        target_disk_mib: u64,
        #[arg(long)]
        confirm_downtime: bool,
        #[arg(long, value_parser = partition_uuid)]
        root_partition_uuid: Option<String>,
    },
    /// Review the capacity needed to expand a stopped VM's disk.
    GrowPlan {
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        target_disk_mib: u64,
        #[arg(long, value_parser = partition_uuid)]
        root_partition_uuid: Option<String>,
    },
    /// Expand a stopped disk using the expected current size and selected root.
    Grow {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        expected_disk_mib: u64,
        #[arg(long)]
        target_disk_mib: u64,
        #[arg(long)]
        confirm_downtime: bool,
        #[arg(long, value_parser = partition_uuid)]
        root_partition_uuid: Option<String>,
    },
    /// Inspect an explicitly scoped unmanaged VM without enrolling it.
    EnrollmentPlan {
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        owner: String,
    },
    /// Enroll only the VM/owner recorded in a reviewed eligible plan JSON file.
    Enroll {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        confirm_enrollment: bool,
    },
    /// Remove retained recovery only after boot/data verification and shutdown.
    FinishShrink {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
        #[arg(long)]
        preparation_id: String,
        #[arg(long)]
        confirm_guest_verified: bool,
        #[arg(long)]
        confirm_delete_recovery: bool,
    },
    Create {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "32")]
        disk_mib: u64,
        #[arg(long, default_value = "128")]
        memory_mib: u64,
        #[arg(long, default_value = "128")]
        min_memory_mib: u64,
        #[arg(long, default_value = "1")]
        cores: u16,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, requires = "ssh_public_key_file")]
        guest_image_sha256: Option<String>,
        #[arg(long, requires = "guest_image_sha256")]
        ssh_public_key_file: Option<PathBuf>,
    },
    Start {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
    },
    Shutdown {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
    },
    Stop {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
    },
    Delete {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
        #[arg(
            long,
            help = "Explicitly acknowledge deletion of this lab VM and its disks"
        )]
        confirm_delete: bool,
    },
}

#[derive(Subcommand)]
enum OperationsCommand {
    /// Inspect fresh evidence and available recovery actions without effects.
    Inspect {
        #[arg(long)]
        operation_id: String,
    },
    /// Apply one action enumerated by a reviewed inspection JSON file.
    Recover {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        inspection: PathBuf,
        #[arg(long, value_enum)]
        kind: RecoveryChoice,
        #[arg(long)]
        confirm_recovery: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RecoveryChoice {
    #[value(alias = "resume_verified_effect")]
    ResumeVerifiedEffect,
    #[value(alias = "abort_preparation")]
    AbortPreparation,
    #[value(alias = "acknowledge_failed_start")]
    AcknowledgeFailedStart,
}
impl From<RecoveryChoice> for RecoveryKind {
    fn from(value: RecoveryChoice) -> Self {
        match value {
            RecoveryChoice::ResumeVerifiedEffect => Self::ResumeVerifiedEffect,
            RecoveryChoice::AbortPreparation => Self::AbortPreparation,
            RecoveryChoice::AcknowledgeFailedStart => Self::AcknowledgeFailedStart,
        }
    }
}

#[derive(Subcommand)]
enum MigrationCommand {
    /// Obtain this controller's public peer descriptor (no private key paths).
    Describe,
    /// Record source authorization for an exact target. This holds resources; it does not start copying.
    Plan {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        vm_id: u16,
        #[arg(long, alias = "destination-descriptor")]
        destination: PathBuf,
        #[arg(long)]
        target_owner: String,
        #[arg(long)]
        target_vm_id: u16,
        #[arg(long, default_value_t = 14400, value_parser = clap::value_parser!(u64).range(1..=MAX_GRANT_SECONDS))]
        expires_in_seconds: u64,
    },
    /// Independently authorize the exact signed source plan at the destination.
    #[command(alias = "authorize")]
    AuthorizeDestination {
        #[arg(long)]
        source_grant: PathBuf,
        #[arg(long, default_value_t = 14400, value_parser = clap::value_parser!(u64).range(1..=MAX_GRANT_SECONDS))]
        expires_in_seconds: u64,
    },
    /// Start or idempotently resume the exact migration approved at both ends.
    Start {
        #[arg(long)]
        migration_id: String,
        #[arg(long)]
        destination_grant: PathBuf,
    },
    List,
    Status {
        #[arg(long)]
        migration_id: String,
    },
    /// Abort only an uncommitted transfer and clean its verified staging resources.
    Abort {
        #[arg(long)]
        migration_id: String,
        #[arg(long)]
        confirm_cleanup: bool,
        /// Source abort proof, or the source Abort/Status RPC output containing it.
        #[arg(long)]
        source_abort: Option<PathBuf>,
    },
    /// Delete retained source recovery only after destination guest verification.
    Finish {
        #[arg(long)]
        migration_id: String,
        #[arg(long)]
        confirm_guest_verified: bool,
        #[arg(long)]
        confirm_delete_recovery: bool,
    },
    /// Renew pre-commit authority for the same immutable plan. At destination, supply the renewed source grant.
    Renew {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        migration_id: String,
        #[arg(long, default_value_t = 14400, value_parser = clap::value_parser!(u64).range(1..=MAX_GRANT_SECONDS))]
        expires_in_seconds: u64,
        #[arg(long)]
        source_grant: Option<PathBuf>,
    },
}

fn partition_uuid(value: &str) -> std::result::Result<String, String> {
    if pve_dev::control::vm_change::valid_partition_uuid(value) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err("expected a complete GPT partition UUID".into())
    }
}

/// Accept either a typed JSON value or the successful RPC envelope printed by
/// this client. Bound reads before allocation; never dump file contents on error.
fn reviewed_value(path: &std::path::Path) -> Result<serde_json::Value> {
    let file =
        fs::File::open(path).with_context(|| format!("open reviewed JSON {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "reviewed JSON must be a regular file"
    );
    ensure!(
        file.metadata()?.len() <= MAX_RESPONSE as u64,
        "reviewed JSON exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_RESPONSE as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RESPONSE,
        "reviewed JSON exceeds size limit"
    );
    let mut value: serde_json::Value =
        serde_json::from_slice(&bytes).context("reviewed file is not valid JSON")?;
    if value.get("ok").is_some() {
        ensure!(
            value.get("ok").and_then(|v| v.as_bool()) == Some(true),
            "reviewed file contains a failed RPC response"
        );
        value = value
            .get("result")
            .cloned()
            .context("reviewed RPC response has no result")?;
    }
    Ok(value)
}

fn reviewed_json<T: DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    serde_json::from_value(reviewed_value(path)?)
        .context("reviewed JSON does not match the expected document type")
}

fn reviewed_abort(path: &std::path::Path, migration_id: &str) -> Result<Signed<AbortReceipt>> {
    let value = reviewed_value(path)?;
    let proof: Signed<AbortReceipt> =
        serde_json::from_value(value.get("abort_proof").cloned().unwrap_or(value))
            .context("reviewed file has no signed source abort proof")?;
    ensure!(
        proof.value.migration_id == migration_id,
        "source abort proof belongs to another migration"
    );
    proof.value.source.validate()?;
    proof
        .verify(&proof.value.source.server_id, "abort")
        .context("source abort proof signature is invalid")?;
    Ok(proof)
}

fn reviewed_grant(path: &std::path::Path, role: MigrationRole) -> Result<Signed<Grant>> {
    let grant: Signed<Grant> = reviewed_json(path)?;
    grant
        .validate_grant(pve_dev::control::store::now(), false)
        .context("reviewed migration authorization is invalid or expired")?;
    ensure!(
        grant.value.role == role,
        "reviewed authorization has the wrong migration role"
    );
    Ok(grant)
}

fn migration_request(command: MigrationCommand) -> Result<MigrationRequest> {
    Ok(match command {
        MigrationCommand::Describe => MigrationRequest::Describe {},
        MigrationCommand::Plan {
            request_id,
            vm_id,
            destination,
            target_owner,
            target_vm_id,
            expires_in_seconds,
        } => {
            let destination: Descriptor = reviewed_json(&destination)?;
            ensure!(
                destination.schema == 1,
                "unsupported reviewed descriptor schema"
            );
            MigrationRequest::Plan {
                request_id,
                vm_id,
                destination,
                target_owner,
                target_vm_id,
                expires_in_seconds,
            }
        }
        MigrationCommand::AuthorizeDestination {
            source_grant,
            expires_in_seconds,
        } => MigrationRequest::AuthorizeDestination {
            source_grant: reviewed_grant(&source_grant, MigrationRole::Source)?,
            expires_in_seconds,
        },
        MigrationCommand::Start {
            migration_id,
            destination_grant,
        } => {
            let destination_grant = reviewed_grant(&destination_grant, MigrationRole::Destination)?;
            ensure!(
                migration_id == destination_grant.value.plan.migration_id,
                "migration ID differs from the reviewed destination grant"
            );
            MigrationRequest::Start {
                migration_id,
                destination_grant,
            }
        }
        MigrationCommand::List => MigrationRequest::List {},
        MigrationCommand::Status { migration_id } => MigrationRequest::Status { migration_id },
        MigrationCommand::Abort {
            migration_id,
            confirm_cleanup,
            source_abort,
        } => {
            ensure!(
                confirm_cleanup,
                "migration abort requires --confirm-cleanup; no request was sent"
            );
            let source_abort = source_abort
                .as_deref()
                .map(|path| reviewed_abort(path, &migration_id))
                .transpose()?;
            MigrationRequest::Abort {
                migration_id,
                confirm_cleanup,
                source_abort,
            }
        }
        MigrationCommand::Finish {
            migration_id,
            confirm_guest_verified,
            confirm_delete_recovery,
        } => {
            ensure!(
                confirm_guest_verified && confirm_delete_recovery,
                "migration finish requires --confirm-guest-verified and --confirm-delete-recovery; no request was sent"
            );
            MigrationRequest::Finish {
                migration_id,
                confirm_guest_verified,
                confirm_delete_recovery,
            }
        }
        MigrationCommand::Renew {
            request_id,
            migration_id,
            expires_in_seconds,
            source_grant,
        } => {
            let source_grant = source_grant
                .as_deref()
                .map(|path| reviewed_grant(path, MigrationRole::Source))
                .transpose()?;
            ensure!(
                source_grant
                    .as_ref()
                    .is_none_or(|grant| grant.value.plan.migration_id == migration_id),
                "migration ID differs from the reviewed renewal grant"
            );
            MigrationRequest::Renew {
                request_id,
                migration_id,
                expires_in_seconds,
                source_grant,
            }
        }
    })
}

fn controller_request(command: Commands) -> Result<Request> {
    let request = match command {
        Commands::Rpc => {
            let mut bytes = Vec::new();
            io::stdin()
                .take(MAX_REQUEST as u64 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_REQUEST, "request exceeds size limit");
            serde_json::from_slice::<Request>(&bytes).context("invalid typed control request")?
        }
        Commands::Dashboard => Request::Dashboard {},
        Commands::Whoami => Request::Whoami {},
        Commands::Budget => Request::Budget {},
        Commands::Images => Request::ListImages {},
        Commands::ImageCatalog => Request::ListImageCatalog {},
        Commands::CreateUser {
            request_id,
            user_id,
            budget_mib,
            min_memory_budget_mib,
            max_memory_budget_mib,
            max_cores,
            max_vms,
        } => Request::CreateUser {
            request_id,
            user_id,
            budget_mib,
            min_memory_budget_mib,
            max_memory_budget_mib,
            max_cores,
            max_vms,
        },
        Commands::UpdateUserPolicy {
            request_id,
            user_id,
            expected_revision,
            budget_mib,
            min_memory_budget_mib,
            max_memory_budget_mib,
            max_cores,
            max_vms,
        } => Request::UpdateUserPolicy {
            request_id,
            user_id,
            expected_revision,
            policy: ResourcePolicy {
                budget_mib,
                min_memory_budget_mib,
                max_memory_budget_mib,
                max_cores,
                max_vms,
            },
        },
        Commands::DeleteUser {
            request_id,
            user_id,
            expected_revision,
        } => Request::DeleteUser {
            request_id,
            user_id,
            expected_revision,
        },
        Commands::EnrollDevice {
            user_id,
            endpoint_id,
            label,
        } => Request::EnrollDevice {
            user_id,
            endpoint_id,
            label,
        },
        Commands::RevokeDevice { endpoint_id } => Request::RevokeDevice { endpoint_id },
        Commands::Operations {
            operation_id,
            command,
        } => {
            ensure!(
                operation_id.is_none() || command.is_none(),
                "choose --operation-id or an operations subcommand, not both"
            );
            match command {
                Some(OperationsCommand::Inspect { operation_id }) => {
                    Request::InspectOperation { operation_id }
                }
                Some(OperationsCommand::Recover {
                    request_id,
                    inspection,
                    kind,
                    confirm_recovery,
                }) => {
                    ensure!(
                        confirm_recovery,
                        "recovery requires --confirm-recovery; no request was sent"
                    );
                    let inspection: OperationInspection = reviewed_json(&inspection)?;
                    let kind = RecoveryKind::from(kind);
                    ensure!(
                        inspection.operation.status
                            == pve_dev::control::protocol::OperationStatus::Blocked
                            && inspection.allowed_actions.contains(&kind),
                        "reviewed inspection does not authorize this recovery action"
                    );
                    Request::RecoverOperation {
                        request_id,
                        operation_id: inspection.operation.operation_id,
                        inspection_id: inspection.inspection_id,
                        kind,
                        confirm_recovery,
                    }
                }
                None => match operation_id {
                    Some(operation_id) => Request::GetOperation { operation_id },
                    None => Request::ListOperations {},
                },
            }
        }
        Commands::Migration { command } => Request::Migration {
            request: Box::new(migration_request(command)?),
        },
        Commands::Vm { command } => match command {
            VmCommand::List => Request::ListVms {},
            VmCommand::ShrinkPlan {
                vm_id,
                target_disk_mib,
                root_partition_uuid,
            } => Request::PlanVmShrink {
                vm_id,
                target_disk_mib,
                root_partition_uuid,
            },
            VmCommand::Shrink {
                request_id,
                vm_id,
                expected_disk_mib,
                target_disk_mib,
                confirm_downtime,
                root_partition_uuid,
            } => {
                ensure!(
                    confirm_downtime,
                    "disk shrink requires --confirm-downtime; no request was sent"
                );
                Request::ShrinkVm {
                    request_id,
                    vm_id,
                    expected_disk_mib,
                    target_disk_mib,
                    confirm_downtime,
                    root_partition_uuid,
                }
            }
            VmCommand::GrowPlan {
                vm_id,
                target_disk_mib,
                root_partition_uuid,
            } => Request::PlanVmGrowth {
                vm_id,
                target_disk_mib,
                root_partition_uuid,
            },
            VmCommand::Grow {
                request_id,
                vm_id,
                expected_disk_mib,
                target_disk_mib,
                confirm_downtime,
                root_partition_uuid,
            } => {
                ensure!(
                    confirm_downtime,
                    "disk growth requires --confirm-downtime; no request was sent"
                );
                Request::GrowVm {
                    request_id,
                    vm_id,
                    expected_disk_mib,
                    target_disk_mib,
                    confirm_downtime,
                    root_partition_uuid,
                }
            }
            VmCommand::EnrollmentPlan { vm_id, owner } => {
                Request::PlanVmEnrollment { vm_id, owner }
            }
            VmCommand::Enroll {
                request_id,
                plan,
                confirm_enrollment,
            } => {
                ensure!(
                    confirm_enrollment,
                    "enrollment requires --confirm-enrollment; no request was sent"
                );
                let plan: EnrollmentPlan = reviewed_json(&plan)?;
                ensure!(
                    plan.eligible && plan.blocked_reasons.is_empty(),
                    "reviewed enrollment plan is not eligible"
                );
                Request::EnrollVm {
                    request_id,
                    plan_id: plan.plan_id,
                    vm_id: plan.vm.vm_id,
                    owner: plan.vm.owner,
                    confirm_enrollment,
                }
            }
            VmCommand::FinishShrink {
                request_id,
                vm_id,
                preparation_id,
                confirm_guest_verified,
                confirm_delete_recovery,
            } => {
                ensure!(
                    confirm_guest_verified && confirm_delete_recovery,
                    "finishing shrink requires --confirm-guest-verified and --confirm-delete-recovery; no request was sent"
                );
                Request::FinishVmShrink {
                    request_id,
                    vm_id,
                    preparation_id,
                    confirm_guest_verified,
                    confirm_delete_recovery,
                }
            }
            VmCommand::Create {
                request_id,
                name,
                disk_mib,
                memory_mib,
                min_memory_mib,
                cores,
                owner,
                guest_image_sha256,
                ssh_public_key_file,
            } => Request::CreateVm {
                request_id,
                name,
                disk_mib,
                memory_mib,
                min_memory_mib,
                cores,
                owner,
                guest: match (guest_image_sha256, ssh_public_key_file) {
                    (Some(image_sha256), Some(path)) => Some(Guest {
                        image_sha256,
                        ssh_public_key: pve_dev::control::guest::public_key_file(
                            &fs::read_to_string(path)?,
                        )?,
                    }),
                    (None, None) => None,
                    _ => anyhow::bail!("guest image and SSH key must be provided together"),
                },
            },
            VmCommand::Start { request_id, vm_id } => Request::VmAction {
                request_id,
                vm_id,
                kind: VmAction::Start,
            },
            VmCommand::Shutdown { request_id, vm_id } => Request::VmAction {
                request_id,
                vm_id,
                kind: VmAction::Shutdown,
            },
            VmCommand::Stop { request_id, vm_id } => Request::VmAction {
                request_id,
                vm_id,
                kind: VmAction::Stop,
            },
            VmCommand::Delete {
                request_id,
                vm_id,
                confirm_delete,
            } => {
                ensure!(
                    confirm_delete,
                    "deletion requires --confirm-delete; no request was sent"
                );
                Request::VmAction {
                    request_id,
                    vm_id,
                    kind: VmAction::Delete,
                }
            }
        },
        Commands::Identity { .. } | Commands::Profile { .. } | Commands::ProfileInfo => {
            anyhow::bail!("local profile/identity commands are not controller requests")
        }
    };
    ensure!(
        serde_json::to_vec(&request)?.len() <= MAX_REQUEST,
        "typed request exceeds size limit"
    );
    Ok(request)
}

async fn run() -> Result<bool> {
    let cli = Cli::parse();
    let request = match cli.command {
        Commands::ProfileInfo => {
            let path = cli
                .profile
                .context("--profile is required for profile-info")?;
            let profile: Profile = serde_json::from_slice(&fs::read(path)?)?;
            println!(
                "{}",
                serde_json::to_string(&transport::profile_binding(&profile)?)?
            );
            return Ok(true);
        }
        Commands::Identity { command } => {
            let key = match command {
                IdentityCommand::New { path } => transport::generate_identity(&path)?,
                IdentityCommand::Show { path } => transport::load_identity(&path)?,
            };
            println!(
                "{}",
                serde_json::json!({"endpoint_id":key.public().to_string()})
            );
            return Ok(true);
        }
        Commands::Profile {
            identity,
            server,
            expected_server,
            output,
            direct_address,
        } => {
            let identity = identity.canonicalize()?;
            transport::load_identity(&identity)?;
            let mut server: Descriptor = serde_json::from_slice(&fs::read(server)?)?;
            ensure!(
                server.schema == 1
                    && server.endpoint.id == expected_server.parse::<iroh::EndpointId>()?,
                "server identity does not match the explicitly trusted endpoint"
            );
            if let Some(address) = direct_address {
                server.endpoint = iroh::EndpointAddr::new(server.endpoint.id).with_ip_addr(address);
            }
            let profile = Profile {
                schema: 1,
                identity,
                server,
            };
            transport::write_new(&output, &serde_json::to_vec_pretty(&profile)?)?;
            println!(
                "{}",
                serde_json::json!({"profile":output,"server_id":profile.server.endpoint.id.to_string(),"environment":profile.server.environment})
            );
            return Ok(true);
        }
        command => controller_request(command)?,
    };
    let path = cli
        .profile
        .context("--profile is required for controller requests")?;
    let profile: Profile = serde_json::from_slice(&fs::read(path)?)?;
    let expected = match (
        cli.expected_server_id,
        cli.expected_environment,
        cli.expected_device_id,
    ) {
        (Some(server_id), Some(environment), Some(device_id)) => Some(ProfileBinding {
            server_id,
            environment,
            device_id,
        }),
        (None, None, None) => None,
        _ => anyhow::bail!("all three expected identity fields are required together"),
    };
    let response = transport::rpc_bound(&profile, request, expected.as_ref()).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(response.ok)
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use pve_dev::control::{migration, transport::Network};
    use serde_json::{Value, json};

    fn request(args: &[&str]) -> Result<Value> {
        let cli = Cli::try_parse_from(std::iter::once("pve-devctl").chain(args.iter().copied()))?;
        Ok(serde_json::to_value(controller_request(cli.command)?)?)
    }
    fn document(value: Value) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reviewed.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        (directory, path)
    }
    fn enrollment() -> Value {
        json!({"plan_id":"exact-plan","vm":{"vm_id":9001,"owner":"trial-user","name":"test-vm","disk_mib":4096,
            "memory_mib":1024,"min_memory_mib":512,"cores":2,"state":"stopped"},"configuration_sha256":"a".repeat(64),
            "root_guid":"11","cloud_init_guid":"12","observed_at":100,"eligible":true,"blocked_reasons":[],"evidence":{}})
    }
    fn grants() -> (Signed<Grant>, Signed<Grant>, Descriptor) {
        let source = iroh::SecretKey::generate();
        let destination = iroh::SecretKey::generate();
        let a = migration::Peer {
            server_id: source.public().to_string(),
            environment: "a".repeat(32),
        };
        let b = migration::Peer {
            server_id: destination.public().to_string(),
            environment: "b".repeat(32),
        };
        let plan = migration::Plan {
            schema: 1,
            migration_id: "1".repeat(64),
            source: a.clone(),
            destination: b.clone(),
            source_vm_id: 9001,
            target_vm_id: 9002,
            source_owner: "trial-user".into(),
            target_owner: "trial-user".into(),
            source_machine_sha256: "2".repeat(64),
            source_config_sha256: "3".repeat(64),
            name: "test-vm".into(),
            disk_mib: 4096,
            memory_mib: 1024,
            min_memory_mib: 512,
            cores: 2,
            guest: None,
            byte_ceiling: 4100 * 1024 * 1024,
            target_bridge: "vmbr77".into(),
        };
        let now = pve_dev::control::store::now();
        let source_grant = Signed::sign(
            Grant {
                schema: 1,
                plan: plan.clone(),
                role: MigrationRole::Source,
                issuer: a.clone(),
                recipient: b.clone(),
                actor: source.public().to_string(),
                nonce: "4".repeat(64),
                source_grant_sha256: None,
                issued_at: now,
                expires_at: now + 3600,
            },
            &source,
            "grant",
        )
        .unwrap();
        let destination_grant = Signed::sign(
            Grant {
                schema: 1,
                plan,
                role: MigrationRole::Destination,
                issuer: b,
                recipient: a,
                actor: destination.public().to_string(),
                nonce: "5".repeat(64),
                source_grant_sha256: Some(migration::digest(&source_grant)),
                issued_at: now,
                expires_at: now + 3600,
            },
            &destination,
            "grant",
        )
        .unwrap();
        let descriptor = Descriptor {
            schema: 1,
            endpoint: iroh::EndpointAddr::new(destination.public())
                .with_ip_addr("127.0.0.1:1234".parse().unwrap()),
            environment: "b".repeat(32),
            network: Network::Loopback,
        };
        (source_grant, destination_grant, descriptor)
    }

    #[test]
    fn resize_parser_preserves_expected_size_root_and_exact_retry_id() {
        let uuid = "0D17BC71-00F4-4E70-8DC2-3E1214773333";
        for (verb, action, target) in [
            ("grow", "grow_vm", "16384"),
            ("shrink", "shrink_vm", "3072"),
        ] {
            let args = [
                "vm",
                verb,
                "--request-id",
                "unchanged-retry",
                "--vm-id",
                "9001",
                "--expected-disk-mib",
                "4096",
                "--target-disk-mib",
                target,
                "--confirm-downtime",
                "--root-partition-uuid",
                uuid,
            ];
            let first = request(&args).unwrap();
            assert_eq!(first, request(&args).unwrap());
            assert_eq!(first["action"], action);
            assert_eq!(first["request_id"], "unchanged-retry");
            assert_eq!(first["expected_disk_mib"], 4096);
            assert_eq!(first["root_partition_uuid"], uuid.to_ascii_lowercase());
            assert_eq!(first["confirm_downtime"], true);
        }
        for (verb, action) in [
            ("grow-plan", "plan_vm_growth"),
            ("shrink-plan", "plan_vm_shrink"),
        ] {
            let value = request(&[
                "vm",
                verb,
                "--vm-id",
                "9001",
                "--target-disk-mib",
                "3072",
                "--root-partition-uuid",
                uuid,
            ])
            .unwrap();
            assert_eq!(value["action"], action);
            assert!(value.get("request_id").is_none());
            assert_eq!(value["root_partition_uuid"], uuid.to_ascii_lowercase());
        }
        assert!(
            request(&[
                "vm",
                "grow-plan",
                "--vm-id",
                "9001",
                "--target-disk-mib",
                "8192",
                "--root-partition-uuid",
                "root"
            ])
            .is_err()
        );
    }

    #[test]
    fn reviewed_enrollment_binds_exact_vm_owner_and_plan() {
        for wrap in [false, true] {
            let value = enrollment();
            let (_directory, path) = document(if wrap {
                json!({"ok":true,"result":value})
            } else {
                value
            });
            let request = request(&[
                "vm",
                "enroll",
                "--request-id",
                "enroll-once",
                "--plan",
                path.to_str().unwrap(),
                "--confirm-enrollment",
            ])
            .unwrap();
            assert_eq!(
                request,
                json!({"action":"enroll_vm","request_id":"enroll-once","plan_id":"exact-plan","vm_id":9001,"owner":"trial-user","confirm_enrollment":true})
            );
        }
        let mut value = enrollment();
        value["eligible"] = json!(false);
        let (_directory, path) = document(value);
        assert!(
            request(&[
                "vm",
                "enroll",
                "--request-id",
                "one",
                "--plan",
                path.to_str().unwrap(),
                "--confirm-enrollment"
            ])
            .is_err()
        );
        assert_eq!(
            request(&[
                "vm",
                "enrollment-plan",
                "--vm-id",
                "9001",
                "--owner",
                "trial-user"
            ])
            .unwrap(),
            json!({"action":"plan_vm_enrollment","vm_id":9001,"owner":"trial-user"})
        );
    }

    #[test]
    fn recovery_uses_only_the_action_in_a_reviewed_blocked_inspection() {
        let value = json!({"operation":{"operation_id":"blocked-one","owner":"trial-user","request_id":"old","kind":"shrink_vm",
            "vm_id":9001,"status":"blocked","step":"shrink_stage","error":"lost acknowledgment","created_at":100},
            "inspection_id":"fresh-proof","observed_at":200,"classification":"verification_required","evidence":[],
            "allowed_actions":["resume_verified_effect"],"proof":{}});
        let (_directory, path) = document(json!({"ok":true,"result":value}));
        let args = [
            "operations",
            "recover",
            "--request-id",
            "recover-once",
            "--inspection",
            path.to_str().unwrap(),
            "--kind",
            "resume-verified-effect",
            "--confirm-recovery",
        ];
        let parsed = request(&args).unwrap();
        assert_eq!(
            parsed,
            json!({"action":"recover_operation","request_id":"recover-once","operation_id":"blocked-one",
            "inspection_id":"fresh-proof","kind":"resume_verified_effect","confirm_recovery":true})
        );
        assert_eq!(parsed, request(&args).unwrap());
        assert!(
            request(&[
                "operations",
                "recover",
                "--request-id",
                "other",
                "--inspection",
                path.to_str().unwrap(),
                "--kind",
                "abort-preparation",
                "--confirm-recovery"
            ])
            .is_err()
        );
        assert_eq!(
            request(&["operations"]).unwrap(),
            json!({"action":"list_operations"})
        );
        assert_eq!(
            request(&["operations", "--operation-id", "one"]).unwrap(),
            json!({"action":"get_operation","operation_id":"one"})
        );
        assert_eq!(
            request(&["operations", "inspect", "--operation-id", "one"]).unwrap(),
            json!({"action":"inspect_operation","operation_id":"one"})
        );
        assert!(
            request(&[
                "operations",
                "--operation-id",
                "one",
                "inspect",
                "--operation-id",
                "two"
            ])
            .is_err()
        );
    }

    #[test]
    fn migration_files_preserve_the_signed_immutable_plan_and_role() {
        let (source, destination, descriptor) = grants();
        let (_source_dir, source_path) = document(json!({"ok":true,"result":source}));
        let (_dest_dir, dest_path) = document(serde_json::to_value(&destination).unwrap());
        let (_route_dir, route_path) = document(json!({"ok":true,"result":descriptor}));
        let plan = request(&[
            "migration",
            "plan",
            "--request-id",
            "plan-once",
            "--vm-id",
            "9001",
            "--destination",
            route_path.to_str().unwrap(),
            "--target-owner",
            "trial-user",
            "--target-vm-id",
            "9002",
        ])
        .unwrap();
        assert_eq!(plan["request"]["kind"], "plan");
        assert_eq!(plan["request"]["request_id"], "plan-once");
        assert_eq!(plan["request"]["expires_in_seconds"], 14400);
        let authorized = request(&[
            "migration",
            "authorize",
            "--source-grant",
            source_path.to_str().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            authorized["request"]["source_grant"],
            serde_json::to_value(&source).unwrap()
        );
        let started = request(&[
            "migration",
            "start",
            "--migration-id",
            &destination.value.plan.migration_id,
            "--destination-grant",
            dest_path.to_str().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            started["request"]["destination_grant"],
            serde_json::to_value(&destination).unwrap()
        );
        assert!(
            request(&[
                "migration",
                "start",
                "--migration-id",
                "another",
                "--destination-grant",
                dest_path.to_str().unwrap()
            ])
            .is_err()
        );
        assert!(
            request(&[
                "migration",
                "start",
                "--migration-id",
                &source.value.plan.migration_id,
                "--destination-grant",
                source_path.to_str().unwrap()
            ])
            .is_err()
        );
    }

    #[test]
    fn renewal_and_read_only_migration_commands_have_exact_payloads() {
        let (source, _, _) = grants();
        let id = &source.value.plan.migration_id;
        let (_directory, path) = document(serde_json::to_value(&source).unwrap());
        let renewal = request(&[
            "migration",
            "renew",
            "--request-id",
            "renew-once",
            "--migration-id",
            id,
            "--source-grant",
            path.to_str().unwrap(),
            "--expires-in-seconds",
            "3600",
        ])
        .unwrap();
        assert_eq!(renewal["request"]["kind"], "renew");
        assert_eq!(renewal["request"]["request_id"], "renew-once");
        assert_eq!(renewal["request"]["migration_id"], id.as_str());
        assert_eq!(renewal["request"]["expires_in_seconds"], 3600);
        let at_source = request(&[
            "migration",
            "renew",
            "--request-id",
            "renew-once",
            "--migration-id",
            id,
        ])
        .unwrap();
        assert!(at_source["request"]["source_grant"].is_null());
        for kind in ["list", "describe"] {
            assert_eq!(
                request(&["migration", kind]).unwrap(),
                json!({"action":"migration","request":{"kind":kind}})
            );
        }
        assert_eq!(
            request(&["migration", "status", "--migration-id", id]).unwrap(),
            json!({"action":"migration","request":{"kind":"status","migration_id":id}})
        );
        assert!(
            request(&[
                "migration",
                "renew",
                "--request-id",
                "renew-once",
                "--migration-id",
                id,
                "--expires-in-seconds",
                "0"
            ])
            .is_err()
        );
        assert!(
            request(&[
                "migration",
                "renew",
                "--request-id",
                "renew-once",
                "--migration-id",
                id,
                "--expires-in-seconds",
                "86401"
            ])
            .is_err()
        );
    }

    #[test]
    fn destination_abort_requires_the_exact_signed_source_abort_document() {
        let key = iroh::SecretKey::generate();
        let id = "1".repeat(64);
        let receipt = AbortReceipt {
            migration_id: id.clone(),
            plan_sha256: "2".repeat(64),
            source: migration::Peer {
                server_id: key.public().to_string(),
                environment: "a".repeat(32),
            },
            aborted_at: pve_dev::control::store::now(),
        };
        let proof = Signed::sign(receipt, &key, "abort").unwrap();
        for wrapped in [false, true] {
            let value = serde_json::to_value(&proof).unwrap();
            let (_directory, path) = document(if wrapped {
                json!({"ok":true,"result":{"phase":"aborted","abort_proof":value}})
            } else {
                value
            });
            let value = request(&[
                "migration",
                "abort",
                "--migration-id",
                &id,
                "--source-abort",
                path.to_str().unwrap(),
                "--confirm-cleanup",
            ])
            .unwrap();
            assert_eq!(
                value["request"]["source_abort"],
                serde_json::to_value(&proof).unwrap()
            );
            assert!(
                request(&[
                    "migration",
                    "abort",
                    "--migration-id",
                    "different",
                    "--source-abort",
                    path.to_str().unwrap(),
                    "--confirm-cleanup"
                ])
                .is_err()
            );
        }
        let mut corrupt = serde_json::to_value(&proof).unwrap();
        corrupt["signature"] = json!("invalid-signature");
        let (_directory, path) = document(corrupt);
        assert!(
            request(&[
                "migration",
                "abort",
                "--migration-id",
                &id,
                "--source-abort",
                path.to_str().unwrap(),
                "--confirm-cleanup"
            ])
            .is_err()
        );
        let source = request(&[
            "migration",
            "abort",
            "--migration-id",
            &id,
            "--confirm-cleanup",
        ])
        .unwrap();
        assert!(source["request"]["source_abort"].is_null());
    }

    #[test]
    fn missing_confirmations_fail_before_reading_any_file_or_profile() {
        for args in [
            vec![
                "vm",
                "grow",
                "--request-id",
                "one",
                "--vm-id",
                "9001",
                "--expected-disk-mib",
                "4096",
                "--target-disk-mib",
                "8192",
            ],
            vec![
                "vm",
                "shrink",
                "--request-id",
                "one",
                "--vm-id",
                "9001",
                "--expected-disk-mib",
                "4096",
                "--target-disk-mib",
                "3072",
            ],
            vec![
                "vm",
                "enroll",
                "--request-id",
                "one",
                "--plan",
                "/not-a-real-plan.json",
            ],
            vec![
                "operations",
                "recover",
                "--request-id",
                "one",
                "--inspection",
                "/not-an-inspection.json",
                "--kind",
                "abort-preparation",
            ],
            vec!["migration", "abort", "--migration-id", "one"],
            vec!["migration", "finish", "--migration-id", "one"],
        ] {
            assert!(
                request(&args)
                    .unwrap_err()
                    .to_string()
                    .contains("requires --confirm")
            );
        }
        assert!(request(&["vm", "delete", "--request-id", "one", "--vm-id", "9001"]).is_err());
    }

    #[test]
    fn reviewed_documents_are_bounded_and_failed_envelopes_are_not_plans() {
        let (_directory, path) =
            document(json!({"ok":false,"error":{"code":"internal","message":"PRIVATE-MESSAGE"}}));
        let error = reviewed_json::<EnrollmentPlan>(&path)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("PRIVATE-MESSAGE"));
        let large = tempfile::NamedTempFile::new().unwrap();
        large.as_file().set_len(MAX_RESPONSE as u64 + 1).unwrap();
        assert!(
            reviewed_json::<EnrollmentPlan>(large.path())
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
    }
}
