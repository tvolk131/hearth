import PveDevCore
import SwiftUI

struct DiskShrinkSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let vm: Vm
    let origin: MutationOrigin?
    let recovery: PendingDiskShrink?
    @State private var rootPartitionText = ""
    @State private var plannedRootPartition: String?
    @State private var target = "3072"
    @State private var confirmed = false
    @State private var removeRecovery = false
    @State private var plan: DiskShrinkPlan?
    @State private var isPlanning = false
    @State private var failure: RpcFailure?

    private var rootSelection: RootPartitionSelection { RootPartitionSelection(text: rootPartitionText) }
    private var targetMib: UInt64? { UInt64(target) }
    private var targetValid: Bool { targetMib.map { $0 >= 2048 && $0 < vm.diskMib } ?? false }
    private var permitted: Bool {
        model.permissions.isAdmin && !model.isReadOnly && vm.state == .stopped
            && model.dashboard?.supports("shrink_vm", legacyLabFallback: true) == true
            && origin == model.requestOrigin
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(recovery == nil ? "Shrink disk for \(vm.name)" : "Finish disk shrink for \(vm.name)")
                .font(.title2.weight(.semibold))
            if let recovery {
                Text("The smaller disk is prepared. Boot the VM, verify your files and services, then shut it down before finishing.")
                Text("Finishing permanently removes the retained original disk and releases the shrink workspace. It does not lower the user's storage budget.")
                    .foregroundStyle(.secondary)
                Toggle("I booted the smaller disk and verified the guest's files and services", isOn: $confirmed)
                Toggle("Remove the original recovery disk", isOn: $removeRecovery)
                Text("Recovery workspace: \(Formatting.mib(recovery.spec.workspaceMib)) remains reserved until removal is verified.")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Text("Prepare a smaller disk for this stopped guest. The original disk is retained until you verify the replacement and explicitly finish the shrink.")
                TextField("New disk size (MiB)", text: $target)
                RootPartitionField(text: $rootPartitionText)
                HStack {
                    Button("Review Shrink Plan") { Task { await loadPlan() } }
                        .disabled(!permitted || !targetValid || !rootSelection.isValid || isPlanning)
                    if isPlanning { ProgressView().controlSize(.small) }
                }
                if let plan, reviewedPlan {
                    Text("Current disk: \(Formatting.mib(vm.diskMib)). Temporary workspace: \(Formatting.mib(plan.workspaceMib)).")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Text("The server checks capacity now and the filesystem during preparation. This capacity plan does not establish filesystem fit. Only supported ext4/GPT disks can be prepared. The user budget stays unchanged.")
                    .font(.callout).foregroundStyle(.secondary)
                Toggle("The guest was shut down cleanly; keep it off throughout preparation", isOn: $confirmed)
            }
            if let failure { Text(failure.detail).foregroundStyle(.red).textSelection(.enabled) }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button(recovery == nil ? "Prepare Smaller Disk" : "Remove Recovery and Finish", role: recovery == nil ? nil : .destructive) {
                    let request: RpcRequest
                    if let recovery {
                        request = .finishVmShrink(FinishVmShrinkRequest(requestId: UUID(), vmId: vm.vmId, preparationId: recovery.operationId,
                            confirmGuestVerified: confirmed, confirmDeleteRecovery: removeRecovery))
                    } else if let targetMib, reviewedPlan {
                        request = .shrinkVm(ShrinkVmRequest(requestId: UUID(), vmId: vm.vmId, expectedDiskMib: vm.diskMib,
                            targetDiskMib: targetMib, confirmDowntime: confirmed, rootPartitionUuid: rootSelection.uuid))
                    } else { return }
                    if model.submit(request, origin: origin) != nil { dismiss() }
                }
                .disabled(!permitted || !confirmed || (recovery == nil ? (!targetValid || !reviewedPlan || isPlanning) : !removeRecovery))
            }
        }
        .padding(24).frame(width: 530)
        .onAppear { if vm.diskMib > 2048 { target = String(max(2048, vm.diskMib - vm.diskMib / 4)) } }
        .onChange(of: target) { _, _ in plan = nil; failure = nil; confirmed = false }
        .onChange(of: rootPartitionText) { _, _ in plan = nil; failure = nil; confirmed = false }
    }

    private var reviewedPlan: Bool {
        plan.map { $0.vmId == vm.vmId && $0.expectedDiskMib == vm.diskMib && $0.targetDiskMib == targetMib && plannedRootPartition == rootSelection.uuid } ?? false
    }

    @MainActor
    private func loadPlan() async {
        guard let requested = targetMib, targetValid, rootSelection.isValid, !isPlanning else { return }
        let selectedRoot = rootSelection.uuid
        isPlanning = true; plan = nil; failure = nil; confirmed = false
        defer { isPlanning = false }
        do throws(RpcFailure) {
            let value = try await model.query(.planVmShrink(vmId: vm.vmId, targetDiskMib: requested, rootPartitionUuid: selectedRoot),
                origin: origin, as: DiskShrinkPlan.self)
            guard targetMib == requested, rootSelection.uuid == selectedRoot, rootSelection.isValid else { return }
            guard value.vmId == vm.vmId, value.expectedDiskMib == vm.diskMib, value.targetDiskMib == requested else {
                throw .malformedResponse("The shrink plan no longer matches this VM and disk size. Refresh and review a new plan.")
            }
            plannedRootPartition = selectedRoot
            plan = value
        } catch { failure = error }
    }
}
