import PveDevCore
import SwiftUI

struct DiskGrowthSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let vm: Vm
    let origin: MutationOrigin?
    @State private var rootPartitionText = ""
    @State private var plannedRootPartition: String?
    @State private var target = ""
    @State private var plan: DiskGrowthPlan?
    @State private var failure: RpcFailure?
    @State private var isPlanning = false
    @State private var confirmed = false

    private var rootSelection: RootPartitionSelection { RootPartitionSelection(text: rootPartitionText) }
    private var targetMib: UInt64? { UInt64(target) }
    private var targetValid: Bool { targetMib.map { $0 > vm.diskMib } ?? false }
    private var permitted: Bool {
        model.permissions.isAdmin && !model.isReadOnly && vm.state == .stopped
            && origin == model.requestOrigin && model.dashboard?.supports("grow_vm") == true
    }
    private var reviewedPlan: Bool {
        plan.map { $0.vmId == vm.vmId && $0.expectedDiskMib == vm.diskMib && $0.targetDiskMib == targetMib && plannedRootPartition == rootSelection.uuid } ?? false
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Expand disk for \(vm.name)").font(.title2.weight(.semibold))
            Text("Current disk: \(Formatting.mib(vm.diskMib)). Shut down the guest cleanly before expanding its disk.")
            TextField("New disk size (MiB)", text: $target)
            RootPartitionField(text: $rootPartitionText)
            HStack {
                Button("Review Expansion Plan") { Task { await loadPlan() } }
                    .disabled(!permitted || !targetValid || !rootSelection.isValid || isPlanning)
                if isPlanning { ProgressView().controlSize(.small) }
            }
            if let plan, reviewedPlan {
                Text("New disk: \(Formatting.mib(plan.targetDiskMib)). Additional storage allocation: \(Formatting.mib(plan.additionalChargeMib)).")
                    .font(.callout)
                Text("The server verifies available budget and capacity. Expanding this disk does not raise the user's storage budget.")
                    .foregroundStyle(.secondary)
                Text("The filesystem is inspected during preparation. This capacity plan does not establish filesystem eligibility.")
                    .foregroundStyle(.secondary)
                Toggle("The guest was shut down cleanly and I authorize this expansion", isOn: $confirmed)
            }
            if let failure { Text(failure.detail).foregroundStyle(.red).textSelection(.enabled) }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Expand Disk") {
                    guard let targetMib, reviewedPlan else { return }
                    let request = GrowVmRequest(requestId: UUID(), vmId: vm.vmId, expectedDiskMib: vm.diskMib,
                        targetDiskMib: targetMib, confirmDowntime: confirmed, rootPartitionUuid: rootSelection.uuid)
                    if model.submit(.growVm(request), origin: origin) != nil { dismiss() }
                }
                .disabled(!permitted || !reviewedPlan || !confirmed || isPlanning)
            }
        }
        .padding(24).frame(width: 530)
        .onAppear {
            let increment = max(1024, vm.diskMib / 4)
            let (next, overflow) = vm.diskMib.addingReportingOverflow(increment)
            target = overflow ? "" : String(next)
        }
        .onChange(of: target) { _, _ in plan = nil; failure = nil; confirmed = false }
        .onChange(of: rootPartitionText) { _, _ in plan = nil; failure = nil; confirmed = false }
    }

    @MainActor
    private func loadPlan() async {
        guard let requested = targetMib, targetValid, rootSelection.isValid, !isPlanning else { return }
        let selectedRoot = rootSelection.uuid
        isPlanning = true; plan = nil; failure = nil; confirmed = false
        defer { isPlanning = false }
        do throws(RpcFailure) {
            let value = try await model.query(.planVmGrowth(vmId: vm.vmId, targetDiskMib: requested, rootPartitionUuid: selectedRoot),
                origin: origin, as: DiskGrowthPlan.self)
            guard targetMib == requested, rootSelection.uuid == selectedRoot, rootSelection.isValid else { return }
            guard value.vmId == vm.vmId, value.expectedDiskMib == vm.diskMib, value.targetDiskMib == requested else {
                throw .malformedResponse("The expansion plan no longer matches this VM and disk size. Refresh and review a new plan.")
            }
            plannedRootPartition = selectedRoot
            plan = value
        } catch { failure = error }
    }
}
