import PveDevCore
import SwiftUI

struct MigrationSheetTarget: Identifiable {
    let id = UUID()
    let vm: Vm
    let origin: MutationOrigin?
    var transfer: MigrationTransferRecord? = nil
}

struct MigrationSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let target: MigrationSheetTarget
    @State private var destinationId: UUID?
    @State private var destinationOrigin: MutationOrigin?
    @State private var descriptor: MigrationDescriptor?
    @State private var targetOwner = ""
    @State private var targetVmId = ""
    @State private var sourceGrant: SignedMigrationGrant?
    @State private var destinationGrant: SignedMigrationGrant?
    @State private var transfer: MigrationTransferRecord?
    @State private var planRequestId = UUID()
    @State private var sourceConfirmed = false
    @State private var destinationConfirmed = false
    @State private var cleanupConfirmed = false
    @State private var guestVerified = false
    @State private var removeRecovery = false
    @State private var isWorking = false
    @State private var startSubmitted = false
    @State private var destinationHasRecord = false
    @State private var destinationCleanupVerified = false
    @State private var destinationCleanupPending = false
    @State private var destinationCleanupConfirmed = false
    @State private var renewConfirmed = false
    @State private var sourceRenewRequestId = UUID()
    @State private var destinationRenewRequestId = UUID()
    @State private var failure: RpcFailure?

    private var source: ServerProfile? { model.settings.servers.first { $0.id == target.origin?.serverId } }
    private var destination: ServerProfile? { model.settings.servers.first { $0.id == destinationId } }
    private var destinations: [ServerProfile] {
        model.settings.servers.filter {
            !$0.isReadOnly && $0.isTrusted && $0.id != target.origin?.serverId
                && $0.identity?.serverId != source?.identity?.serverId && $0.identity?.environment != source?.identity?.environment
        }
    }
    private var destinationUsers: [User] {
        destinationId.flatMap { model.dashboard(forServer: $0)?.users.filter { $0.state == .active && $0.ready } } ?? []
    }
    private var sourceCurrent: Bool {
        target.origin == model.requestOrigin && model.permissions.isAdmin && !model.isReadOnly
            && model.dashboard?.supports("migration") == true
    }
    private var destinationCurrent: Bool {
        guard let destinationOrigin, let destination else { return false }
        return destinationOrigin == model.origin(forServer: destination.id) && !destination.isReadOnly
            && Permissions(identity: model.dashboard(forServer: destination.id)?.identity).isAdmin
            && model.dashboard(forServer: destination.id)?.supports("migration") == true
    }

    var body: some View {
        ScrollView {
        VStack(alignment: .leading, spacing: 14) {
            Text("Move \(target.vm.name) to another server").font(.title2.weight(.semibold))
            Text("Authorize the same stopped-VM transfer independently at both servers. The source recovery copy remains fenced until you verify the destination and explicitly finish.")
                .foregroundStyle(.secondary)
            if let source { Text("Source: \(source.name) · VM \(target.vm.vmId) · \(Formatting.mib(target.vm.diskMib))") }
            if sourceGrant == nil {
                Form {
                    Picker("Destination server", selection: $destinationId) {
                        Text("Choose a server").tag(Optional<UUID>.none)
                        ForEach(destinations) { server in Text(server.name).tag(Optional(server.id)) }
                    }
                    .disabled(isWorking)
                    Button("Load Destination") { Task { await loadDestination() } }
                        .disabled(destinationId == nil || isWorking || !sourceCurrent)
                    Picker("Destination owner", selection: $targetOwner) {
                        Text("Choose an owner").tag("")
                        ForEach(destinationUsers) { user in Text(user.userId).tag(user.userId) }
                    }
                    TextField("Destination VM ID", text: $targetVmId)
                }
                .disabled(isWorking)
                if descriptor != nil {
                    Toggle("Authorize this VM, destination, owner and VM ID at the source", isOn: $sourceConfirmed)
                    Button("Authorize at Source") { Task { await authorizeSource() } }
                        .disabled(!sourceCurrent || !destinationCurrent || !sourceConfirmed || targetOwner.isEmpty || UInt16(targetVmId) == nil || isWorking)
                }
            } else if let sourceGrant {
                GroupBox("Reviewed transfer") {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("\(source?.name ?? "Source") → \(destination?.name ?? "Destination")")
                        Text("VM \(sourceGrant.value.plan.sourceVmId) → VM \(sourceGrant.value.plan.targetVmId), owner \(sourceGrant.value.plan.targetOwner)")
                        Text("Disk \(Formatting.mib(sourceGrant.value.plan.diskMib)) · \(sourceGrant.value.plan.cores) cores · \(Formatting.mib(sourceGrant.value.plan.memoryMib)) memory")
                        Text("Destination network: \(SafeText.sanitize(sourceGrant.value.plan.targetBridge, limit: 80))")
                        Text("Transfer: \(SafeText.sanitize(sourceGrant.value.plan.migrationId, limit: 64))").font(.caption.monospaced()).textSelection(.enabled)
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
                if !sourceGrant.isFresh() {
                    Text("The source authorization expired. Renew authorization for this same transfer before continuing.")
                        .foregroundStyle(.orange)
                }
                if let destinationGrant, !destinationGrant.isFresh() {
                    Text("The destination authorization expired. This grant cannot start a new copy.").foregroundStyle(.orange)
                }
                if transfer == nil || ((transfer?.phase == "authorized" || transfer?.phase == "fenced") && !startSubmitted) {
                    if !destinationCurrent {
                        Button("Connect to Reviewed Destination") { Task { await loadDestination() } }
                            .disabled(destinationId == nil || isWorking || !sourceCurrent)
                    }
                    if destinationGrant == nil {
                        Toggle("Authorize this exact transfer at the destination", isOn: $destinationConfirmed)
                        Button("Authorize at Destination") { Task { await authorizeDestination() } }
                            .disabled(!sourceCurrent || !destinationCurrent || !destinationConfirmed || !sourceGrant.isFresh() || isWorking)
                    } else {
                        Text("Both servers authorized this transfer. The daemon will continue the copy if this window closes.").foregroundStyle(.secondary)
                        Button("Start Stopped Migration") { Task { await start() } }
                            .disabled(!sourceCurrent || !destinationCurrent || !sourceGrant.isFresh() || destinationGrant?.isFresh() != true || isWorking)
                    }
                }
            }
            if let transfer {
                MigrationProgress(record: transfer)
                HStack {
                    Button("Refresh Transfer") { Task { await refreshTransfer() } }.disabled(isWorking || !sourceCurrent)
                    if transfer.phase == "blocked" { Text("Needs investigation").foregroundStyle(.red) }
                }
                if transfer.mayRenew {
                    Toggle("Renew the same transfer at source; require fresh destination consent", isOn: $renewConfirmed)
                    Button("Renew Source Authorization") { Task { await renewSource() } }
                        .disabled(!sourceCurrent || !renewConfirmed || isWorking)
                }
                if transfer.phase == "aborted", transfer.abortProof?.matches(transfer.plan) == true {
                    if destinationCleanupVerified {
                        Text("Destination staging cleanup verified.").foregroundStyle(.secondary)
                    } else if destinationCleanupPending {
                        Text("Destination staging cleanup is queued. Waiting for its verified receipt…").foregroundStyle(.secondary)
                        Button("Refresh Destination Cleanup") { Task { await refreshDestinationCleanup() } }
                            .disabled(!sourceCurrent || !destinationCurrent || isWorking)
                    } else {
                        if !destinationCurrent {
                            Button("Connect to Reviewed Destination") { Task { await loadDestination() } }
                                .disabled(destinationId == nil || !sourceCurrent || isWorking)
                        }
                        Toggle("Clean destination staging using the signed source abort proof", isOn: $destinationCleanupConfirmed)
                        Button("Clean Up Destination Staging", role: .destructive) { Task { await cleanupDestination() } }
                            .disabled(!sourceCurrent || !destinationCurrent || !destinationCleanupConfirmed || isWorking)
                    }
                }
                if transfer.mayAbort {
                    Toggle("Abort this uncommitted transfer and clean up only its staging resources", isOn: $cleanupConfirmed)
                    Button("Abort Transfer", role: .destructive) { Task { await abort() } }
                        .disabled(!sourceCurrent || !cleanupConfirmed || isWorking)
                }
                if transfer.mayFinish {
                    Toggle("I verified the destination guest's files and services", isOn: $guestVerified)
                    Toggle("Permanently remove the source recovery copy", isOn: $removeRecovery)
                    Button("Remove Recovery and Finish", role: .destructive) { Task { await finish() } }
                        .disabled(!sourceCurrent || !guestVerified || !removeRecovery || isWorking)
                }
            }
            if let failure { Text(failure.detail).foregroundStyle(.red).textSelection(.enabled) }
            HStack {
                if isWorking { ProgressView().controlSize(.small); Text("Waiting for server…").foregroundStyle(.secondary) }
                Spacer()
                Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
            }
        }
        .padding(24)
        }
        .frame(width: 680).frame(maxHeight: 720)
        .onAppear { restore() }
        .onChange(of: destinationId) { _, _ in
            descriptor = nil; destinationOrigin = nil
            if sourceGrant == nil { targetOwner = ""; sourceConfirmed = false; planRequestId = UUID() }
        }
        .onChange(of: targetOwner) { _, _ in sourceConfirmed = false; planRequestId = UUID() }
        .onChange(of: targetVmId) { _, _ in sourceConfirmed = false; planRequestId = UUID() }
        .task(id: transfer.map { $0.id + "-" + $0.phase + (startSubmitted ? "-started" : "-review") }) {
            guard transfer != nil else { return }
            while !Task.isCancelled, let current = transfer, !current.isTerminal, current.phase != "blocked", sourceCurrent,
                  (["preparing", "fenced", "receiving", "ready", "relinquished", "accepted", "cleaning"].contains(current.phase)
                    || (current.phase == "authorized" && startSubmitted)) {
                do { try await Task.sleep(for: .seconds(5)); try Task.checkCancellation() } catch { return }
                await refreshTransfer()
                if failure != nil { return }
            }
        }
        .task(id: destinationCleanupPending ? transfer?.id : nil) {
            while destinationCleanupPending, sourceCurrent, destinationCurrent, !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(5)); try Task.checkCancellation() } catch { return }
                await refreshDestinationCleanup()
                if failure != nil { return }
            }
        }
    }

    private func restore() {
        targetVmId = String(target.vm.vmId)
        guard let record = target.transfer else { return }
        transfer = record; sourceGrant = record.sourceGrant; destinationGrant = record.destinationGrant
        startSubmitted = record.phase != "authorized" && record.destinationGrant != nil
        destinationId = destinations.first { $0.identity.map { record.plan.destination.matches($0) } == true }?.id
        if let destinationId { destinationOrigin = model.origin(forServer: destinationId) }
        targetOwner = record.plan.targetOwner; targetVmId = String(record.plan.targetVmId)
    }

    @MainActor
    private func loadDestination() async {
        guard let destinationId, let origin = model.origin(forServer: destinationId), !isWorking else { return }
        isWorking = true; failure = nil; descriptor = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let dashboard = try await model.loadDashboard(on: origin)
            guard sourceCurrent, self.destinationId == destinationId else { throw .configuration("The selected servers changed. Review this transfer again.") }
            guard dashboard.identity.role.isAdmin, dashboard.identity.state == .active, dashboard.supports("migration") else {
                throw .configuration("The destination requires an administrator profile with migration enabled.")
            }
            let value = try await model.queryOnRoute(.migration(.describe), origin: origin, as: MigrationDescriptor.self)
            guard let identity = destination?.identity, value.matches(identity) else {
                throw .malformedResponse("The destination route does not match its pinned server identity.")
            }
            destinationOrigin = origin; descriptor = value
            if let sourceGrant {
                let records = try await model.queryOnRoute(.migration(.list), origin: origin, as: [MigrationTransferRecord].self)
                if let existing = records.first(where: { $0.id == sourceGrant.value.plan.migrationId }) {
                    guard existing.role == "destination", existing.plan == sourceGrant.value.plan else {
                        throw .malformedResponse("The destination transfer belongs to another plan.")
                    }
                    destinationHasRecord = true
                    if existing.sourceGrant == sourceGrant, let grant = existing.destinationGrant,
                       MigrationReview.destinationGrant(grant, matches: sourceGrant, destination: identity), grant.isFresh() {
                        destinationGrant = grant
                    }
                    if let proof = transfer?.abortProof,
                       let state = DestinationCleanupState.review(existing, plan: sourceGrant.value.plan, sourceProof: proof) {
                        destinationCleanupVerified = state == .verified
                        destinationCleanupPending = state == .pending
                    }
                } else { destinationHasRecord = false }
            }
        } catch { failure = error }
    }

    @MainActor
    private func authorizeSource() async {
        guard let sourceOrigin = target.origin, let destinationOrigin, let descriptor, let id = UInt16(targetVmId),
              let sourceIdentity = source?.identity, let destinationIdentity = destination?.identity,
              sourceCurrent, destinationCurrent, !isWorking else { return }
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let reply = try await model.performMutation(.migration(.plan(requestId: planRequestId, vmId: target.vm.vmId,
                destination: descriptor, targetOwner: targetOwner, targetVmId: id, expiresInSeconds: 14400)), origin: sourceOrigin)
            guard sourceCurrent, destinationOrigin == model.origin(forServer: destinationOrigin.serverId),
                  case .migrationGrant(let value) = reply,
                  MigrationReview.sourceGrant(value, matches: target.vm, source: sourceIdentity, destination: destinationIdentity,
                                              targetVmId: id, targetOwner: targetOwner) else {
                throw .malformedResponse("The source authorization no longer matches the reviewed VM and destination. Refresh its transfer list before continuing.")
            }
            sourceGrant = value
            let record = try await model.query(.migration(.status(migrationId: value.value.plan.migrationId)),
                origin: sourceOrigin, as: MigrationTransferRecord.self)
            guard record.plan == value.value.plan, record.role == "source" else {
                throw .malformedResponse("The stored transfer differs from its source authorization.")
            }
            transfer = record
        } catch { failure = error }
    }

    @MainActor
    private func authorizeDestination() async {
        guard let origin = destinationOrigin, let sourceGrant, let identity = destination?.identity,
              sourceCurrent, destinationCurrent, !isWorking else { return }
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let action: MigrationAdminRequest = destinationHasRecord
                ? .renew(requestId: destinationRenewRequestId, migrationId: sourceGrant.value.plan.migrationId, expiresInSeconds: 14400, sourceGrant: sourceGrant)
                : .authorizeDestination(sourceGrant: sourceGrant, expiresInSeconds: 14400)
            let reply = try await model.performMutation(.migration(action), origin: origin)
            guard sourceCurrent, case .migrationGrant(let value) = reply,
                  MigrationReview.destinationGrant(value, matches: sourceGrant, destination: identity) else {
                throw .malformedResponse("The destination authorization differs from the reviewed transfer.")
            }
            destinationGrant = value; destinationHasRecord = true; destinationRenewRequestId = UUID()
        } catch { failure = error }
    }

    @MainActor
    private func renewSource() async {
        guard let origin = target.origin, let transfer, transfer.mayRenew, renewConfirmed,
              let sourceIdentity = source?.identity, let destinationIdentity = destination?.identity, !isWorking else { return }
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let reply = try await model.performMutation(.migration(.renew(requestId: sourceRenewRequestId, migrationId: transfer.id,
                expiresInSeconds: 14400)), origin: origin)
            guard case .migrationGrant(let grant) = reply,
                  MigrationReview.sourceGrant(grant, matches: target.vm, source: sourceIdentity, destination: destinationIdentity,
                    targetVmId: transfer.plan.targetVmId, targetOwner: transfer.plan.targetOwner) else {
                throw .malformedResponse("The renewed authorization differs from the reviewed transfer.")
            }
            let value = try await model.query(.migration(.status(migrationId: transfer.id)), origin: origin, as: MigrationTransferRecord.self)
            guard value.plan == transfer.plan, value.role == "source" else { throw .malformedResponse("The renewal status belongs to another transfer.") }
            self.transfer = value; sourceGrant = value.sourceGrant; destinationGrant = value.destinationGrant
            sourceRenewRequestId = UUID(); renewConfirmed = false; destinationConfirmed = false; startSubmitted = false
            if value.sourceGrant != grant {
                throw .configuration("A later authorization already exists. Its current state was loaded; review it before continuing.")
            }
        } catch { failure = error }
    }

    @MainActor
    private func cleanupDestination() async {
        guard let transfer, let proof = transfer.abortProof, proof.matches(transfer.plan), let origin = destinationOrigin,
              sourceCurrent, destinationCurrent, destinationCleanupConfirmed, !isWorking else { return }
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let reply = try await model.performMutation(.migration(.abort(migrationId: transfer.id, confirmCleanup: true, sourceAbort: proof)), origin: origin)
            guard case .migrationTransfer(let value) = reply,
                  let state = DestinationCleanupState.review(value, plan: transfer.plan, sourceProof: proof) else {
                throw .malformedResponse("Destination cleanup did not return the reviewed source abort proof.")
            }
            destinationCleanupVerified = state == .verified
            destinationCleanupPending = state == .pending
        } catch { failure = error }
    }

    @MainActor
    private func refreshDestinationCleanup() async {
        guard let transfer, let proof = transfer.abortProof, let origin = destinationOrigin,
              sourceCurrent, destinationCurrent, !isWorking else { return }
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let value = try await model.queryOnRoute(.migration(.status(migrationId: transfer.id)), origin: origin,
                as: MigrationTransferRecord.self)
            guard let state = DestinationCleanupState.review(value, plan: transfer.plan, sourceProof: proof) else {
                throw .malformedResponse("Destination status does not prove cleanup of this reviewed transfer.")
            }
            destinationCleanupVerified = state == .verified
            destinationCleanupPending = state == .pending
        } catch { failure = error }
    }

    @MainActor
    private func start() async {
        guard let origin = target.origin, let grant = destinationGrant, let plan = sourceGrant?.value.plan,
              sourceCurrent, destinationCurrent, !isWorking else { return }
        await mutate(.start(migrationId: plan.migrationId, destinationGrant: grant), origin: origin, plan: plan)
    }

    @MainActor
    private func abort() async {
        guard let origin = target.origin, let transfer, transfer.mayAbort, cleanupConfirmed, !isWorking else { return }
        await mutate(.abort(migrationId: transfer.id, confirmCleanup: true), origin: origin, plan: transfer.plan)
    }

    @MainActor
    private func finish() async {
        guard let origin = target.origin, let transfer, transfer.mayFinish, guestVerified, removeRecovery, !isWorking else { return }
        await mutate(.finish(migrationId: transfer.id, confirmGuestVerified: true, confirmDeleteRecovery: true), origin: origin, plan: transfer.plan)
    }

    @MainActor
    private func mutate(_ action: MigrationAdminRequest, origin: MutationOrigin, plan: MigrationPlan) async {
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let reply = try await model.performMutation(.migration(action), origin: origin)
            guard case .migrationTransfer(let value) = reply, value.plan == plan, value.role == "source" else {
                throw .malformedResponse("The transfer receipt belongs to another migration.")
            }
            transfer = value
            if case .start = action { startSubmitted = true }
        } catch { failure = error }
    }

    @MainActor
    private func refreshTransfer() async {
        guard !isWorking, let transfer, let origin = target.origin else { return }
        isWorking = true; failure = nil
        defer { isWorking = false }
        do throws(RpcFailure) {
            let value = try await model.query(.migration(.status(migrationId: transfer.id)), origin: origin, as: MigrationTransferRecord.self)
            guard value.plan == transfer.plan, value.role == "source" else { throw .malformedResponse("The status belongs to another transfer.") }
            self.transfer = value
            if sourceGrant != value.sourceGrant {
                sourceGrant = value.sourceGrant; destinationGrant = value.destinationGrant
                destinationConfirmed = false; renewConfirmed = false
                startSubmitted = value.destinationGrant != nil
            }
        } catch { failure = error }
    }
}

struct MigrationProgress: View {
    let record: MigrationTransferRecord
    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text("\(record.role.capitalized): \(SafeText.sanitize(record.phase.replacingOccurrences(of: "_", with: " "), limit: 80))").fontWeight(.medium)
            if let bytes = record.transferredBytes { Text("\(bytes.formatted()) bytes transferred").font(.caption).monospacedDigit().foregroundStyle(.secondary) }
            if let error = record.error { Text(SafeText.sanitize(error, limit: 1200)).foregroundStyle(.red).textSelection(.enabled) }
        }
    }
}
