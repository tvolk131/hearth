import PveDevCore
import SwiftUI

struct EnrollmentSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let origin: MutationOrigin?
    @State private var vmId = ""
    @State private var owner = ""
    @State private var plan: EnrollmentPlan?
    @State private var failure: RpcFailure?
    @State private var isPlanning = false
    @State private var confirmed = false

    private var owners: [User] { model.dashboard?.users.filter { $0.ready && $0.state == .active } ?? [] }
    private var numericVmId: UInt16? { UInt16(vmId) }
    private var permitted: Bool {
        model.permissions.isAdmin && !model.isReadOnly && origin == model.requestOrigin
            && model.dashboard?.supports("plan_vm_enrollment") == true
            && model.dashboard?.supports("enroll_vm") == true
    }
    private var matchingPlan: Bool {
        plan.map { $0.vm.vmId == numericVmId && $0.vm.owner == owner && $0.eligible && $0.blockedReasons.isEmpty } ?? false
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Enroll an existing virtual machine").font(.title2.weight(.semibold))
            Text("Review a VM that this server explicitly permits for enrollment, then assign it to an owner and their resource budget.")
                .foregroundStyle(.secondary)
            Form {
                TextField("VM ID", text: $vmId)
                Picker("Owner", selection: $owner) {
                    Text("Choose an owner").tag("")
                    ForEach(owners) { user in Text(user.userId).tag(user.userId) }
                }
            }
            Button("Review Enrollment Plan") { Task { await loadPlan() } }
                .disabled(!permitted || numericVmId == nil || owner.isEmpty || isPlanning)
            if isPlanning { ProgressView("Inspecting VM and allocation…") }
            if let plan {
                GroupBox("Observed VM") {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("\(plan.vm.name) · VM \(plan.vm.vmId)").fontWeight(.medium)
                        Text("Disk \(Formatting.mib(plan.vm.diskMib)) · Memory \(Formatting.mib(plan.vm.memoryMib)) · \(plan.vm.cores) cores")
                        Text("Owner: \(plan.vm.owner)")
                        Text("Root disk identity: \(SafeText.sanitize(plan.rootGuid, limit: 80))")
                        Text("Cloud-init disk identity: \(SafeText.sanitize(plan.cloudInitGuid, limit: 80))")
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .textSelection(.enabled)
                }
                ForEach(Array(plan.blockedReasons.prefix(32).enumerated()), id: \.offset) { _, reason in
                    Text(SafeText.sanitize(reason, limit: 600)).foregroundStyle(.red)
                }
                if matchingPlan {
                    Toggle("Enroll this VM under \(owner) and charge its resources to that owner", isOn: $confirmed)
                }
            }
            if let failure { Text(failure.detail).foregroundStyle(.red).textSelection(.enabled) }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Enroll VM") {
                    guard let plan, let numericVmId, matchingPlan else { return }
                    let request = EnrollVmRequest(requestId: UUID(), planId: plan.planId, vmId: numericVmId,
                        owner: owner, confirmEnrollment: confirmed)
                    if model.submit(.enrollVm(request), origin: origin) != nil { dismiss() }
                }
                .disabled(!permitted || !matchingPlan || !confirmed || isPlanning)
            }
        }
        .padding(24).frame(width: 600)
        .onChange(of: vmId) { _, _ in clearPlan() }
        .onChange(of: owner) { _, _ in clearPlan() }
    }

    private func clearPlan() { plan = nil; failure = nil; confirmed = false }

    @MainActor
    private func loadPlan() async {
        guard let requestedVm = numericVmId, !owner.isEmpty, !isPlanning else { return }
        let requestedOwner = owner
        isPlanning = true; clearPlan()
        defer { isPlanning = false }
        do throws(RpcFailure) {
            let value = try await model.query(.planVmEnrollment(vmId: requestedVm, owner: requestedOwner),
                origin: origin, as: EnrollmentPlan.self)
            guard numericVmId == requestedVm, owner == requestedOwner else { return }
            guard value.vm.vmId == requestedVm, value.vm.owner == requestedOwner else {
                throw .malformedResponse("The enrollment plan belongs to another VM or owner.")
            }
            plan = value
        } catch { failure = error }
    }
}
