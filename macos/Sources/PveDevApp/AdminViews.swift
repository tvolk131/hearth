import PveDevCore
import SwiftUI

struct AdminOnly<Content: View>: View {
  @Environment(AppModel.self) private var model
  @ViewBuilder let content: () -> Content

  var body: some View {
    if model.permissions.isAdmin {
      content()
    } else {
      ContentUnavailableView(
        "Administrator access required", systemImage: "lock",
        description: Text(
          "The server decides permissions. This view is shown only to administrators."))
    }
  }
}

struct AdminUsersView: View {
  let origin: MutationOrigin?
  @Environment(AppModel.self) private var model
  @State private var draft = CreateUserDraft()
  @State private var issues: [String] = []
  @State private var deleteIssues: [String] = []
  @State private var creatingUser: RpcRequest?
  @State private var editingUser: User?
  @State private var deletingUser: DeleteUserRequest?

  var body: some View {
    GeometryReader { geometry in
      ScrollView {
        VStack(alignment: .leading, spacing: 16) {
          AdminOnly {
            DashboardGate { dashboard in
              usersTable(dashboard)
              IssueList(messages: deleteIssues)
              createUserForm
            }
          }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
      }
      .frame(width: geometry.size.width, height: geometry.size.height, alignment: .topLeading)
    }
    .navigationTitle("Users")
    .confirmationDialog(
      "Create user on \(model.serverName(on: origin))?",
      isPresented: Binding(get: { creatingUser != nil }, set: { if !$0 { creatingUser = nil } }),
      presenting: creatingUser
    ) { request in
      Button("Create User") {
        if model.submit(request, origin: origin) != nil { draft = CreateUserDraft() }
      }
      Button("Cancel", role: .cancel) {}
    } message: { request in
      Text("\(request.summary) on \(model.serverName(on: origin)).")
    }
    .sheet(item: $editingUser) { user in
      EditUserPolicySheet(user: user, vms: model.dashboard?.namespaceVms ?? [], origin: origin)
    }
    .confirmationDialog(
      "Delete this empty account on \(model.serverName(on: origin))?",
      isPresented: Binding(get: { deletingUser != nil }, set: { if !$0 { deletingUser = nil } }),
      presenting: deletingUser
    ) { request in
      Button("Delete Empty Account", role: .destructive) {
        _ = model.submit(.deleteUser(request), origin: origin)
      }
      Button("Cancel", role: .cancel) {}
    } message: { request in
      Text(
        "On \(model.serverName(on: origin)), delete \(request.userId), revoke all its devices, and remove its verified-empty storage allocation. VMs are never deleted by this action. The server refuses existing resources, unfinished work or unexpected storage. The user ID remains retired."
      )
    }
  }

  private func usersTable(_ dashboard: DashboardResult) -> some View {
    SectionCard(title: "Users", systemImage: "person.2") {
      ScrollView(.horizontal) {
        Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 8) {
          GridRow {
            Text("User")
            Text("Role")
            Text("Disk · all allocations")
            Text("Memory · all allocations")
            Text("Cores · total")
            Text("VMs · total")
            Text("State")
            Text("Actions")
          }
          .font(.caption.weight(.semibold))
          .foregroundStyle(.secondary)
          Divider().gridCellUnsizedAxes(.horizontal)
          ForEach(dashboard.users) { user in
            let usage = OwnedUsage(vms: dashboard.vms, owner: user.userId)
            let compute = dashboard.totalComputeBudget(for: user)
            let retainedRecovery = dashboard.diskShrinks.contains { $0.owner == user.userId }
            GridRow {
              Text(user.userId).fontWeight(.medium)
              RoleBadge(role: user.role)
              VStack(alignment: .leading, spacing: 3) {
                Text(
                  user.state == .deleted
                    ? "Released" : Formatting.mib(dashboard.totalDiskBudget(for: user)))
                if user.state != .deleted {
                  Text("New VMs: \(Formatting.mib(user.budgetMib)) · \(user.maxVms) slots").font(
                    .caption
                  ).foregroundStyle(.secondary)
                }
              }.monospacedDigit()
              Text(
                user.state == .deleted
                  ? "Released"
                  : "\(Formatting.mib(compute.minimum))–\(Formatting.mib(compute.maximum))"
              )
              .monospacedDigit()
              Text(user.state == .deleted ? "—" : String(compute.cores)).monospacedDigit()
              Text(user.state == .deleted ? "0" : "\(usage.activeVmCount)/\(compute.vms)")
                .monospacedDigit()
              Text(
                retainedRecovery
                  ? "Shrink recovery retained"
                  : (user.state == .active
                    ? (user.ready ? "Ready" : "Pending") : user.state.rawValue.capitalized)
              )
              .foregroundStyle(
                user.ready && user.state == .active && !retainedRecovery ? .green : .secondary)
              HStack {
                Button("Edit Allocation…") { editingUser = user }.disabled(
                  !user.ready || user.state != .active || retainedRecovery)
                if user.role == .member && user.state != .deleted {
                  Button("Delete…", role: .destructive) {
                    switch DeleteUserDraft(user: user).build(
                      requestId: UUID(), vms: dashboard.vms, operations: dashboard.operations)
                    {
                    case .success(.deleteUser(let request)):
                      deleteIssues = []
                      deletingUser = request
                    case .failure(let failure): deleteIssues = failure.messages
                    default: break
                    }
                  }.disabled(!user.ready || user.state != .active || retainedRecovery)
                }
              }
            }
          }
        }.fixedSize(horizontal: true, vertical: true)
      }
      .fixedSize(horizontal: false, vertical: true)
      .frame(maxWidth: .infinity, alignment: .leading)
    }
  }

  private var createUserForm: some View {
    SectionCard(
      title: "Create User on \(model.serverName(on: origin))", systemImage: "person.badge.plus"
    ) {
      VStack(alignment: .leading, spacing: 10) {
        Form {
          TextField("User ID", text: $draft.userId, prompt: Text("frank"))
          TextField("Disk budget (MiB)", text: $draft.budgetMib)
          TextField("Minimum memory budget (MiB)", text: $draft.minMemoryBudgetMib)
          TextField("Maximum memory budget (MiB)", text: $draft.maxMemoryBudgetMib)
          TextField("Max cores", text: $draft.maxCores)
          TextField("Max VMs", text: $draft.maxVms)
        }
        .formStyle(.grouped)
        .frame(maxWidth: 520)
        IssueList(messages: issues)
        HStack {
          Button("Create User") { submit() }
          Text("Creates a user record and a budget for new managed resources.")
            .font(.caption)
            .foregroundStyle(.secondary)
        }
      }
    }
  }

  private func submit() {
    switch draft.build(requestId: UUID()) {
    case .failure(let failure):
      issues = failure.messages
    case .success(let request):
      issues = []
      creatingUser = request
    }
  }
}

struct EditUserPolicySheet: View {
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  let vms: [Vm]
  let origin: MutationOrigin?
  @State private var draft: UpdateUserPolicyDraft
  @State private var issues: [String] = []
  @State private var candidate: RpcRequest?

  init(user: User, vms: [Vm], origin: MutationOrigin?) {
    self.vms = vms
    self.origin = origin
    _draft = State(initialValue: UpdateUserPolicyDraft(user: user))
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 16) {
      Text(
        "Edit provisioning allocation for \(draft.previous.userId) on \(model.serverName(on: origin))"
      ).font(.title2.bold())
      if let allocation = model.dashboard?.storageAllocations.first(where: {
        $0.owner == draft.previous.userId && $0.acceptsNewVms
      }) {
        Text(allocation.dataset).font(.caption.monospaced()).textSelection(.enabled)
        Text(
          "Editing disk, memory, cores and slots for the new-VM allocation: \(Formatting.mib(draft.previous.budgetMib)) · \(draft.previous.maxVms) slots. Existing allocations: \(Formatting.mib(max(model.dashboard?.totalDiskBudget(for: draft.previous) ?? draft.previous.budgetMib, draft.previous.budgetMib) - draft.previous.budgetMib)) (fixed, root-managed). Other allocations keep their current budgets."
        )
        .font(.callout).foregroundStyle(.secondary)
      }
      Text(
        "Current policy revision \(draft.previous.policyRevision). Reductions release only unused allocation; thick disks, stopped VMs and unfinished work still count. VM disks and data are never shrunk or removed."
      )
      .foregroundStyle(.secondary)
      Form {
        TextField("Disk budget (MiB)", text: $draft.fields.budgetMib)
        TextField("Minimum memory budget (MiB)", text: $draft.fields.minMemoryBudgetMib)
        TextField("Maximum memory budget (MiB)", text: $draft.fields.maxMemoryBudgetMib)
        TextField("Max cores", text: $draft.fields.maxCores)
        TextField("Max VMs", text: $draft.fields.maxVms)
      }.formStyle(.grouped)
      IssueList(messages: issues)
      Text(
        "The server rechecks current commitments, actual ZFS usage and capacity. New VM actions for this owner are paused while the change is pending; uncertain outcomes remain blocked for investigation."
      )
      .font(.caption).foregroundStyle(.secondary)
      HStack {
        Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
        Spacer()
        Button("Review Change…") {
          switch draft.build(requestId: UUID(), vms: vms, backend: model.dashboard?.backend ?? .lab)
          {
          case .failure(let failure): issues = failure.messages
          case .success(let request):
            issues = []
            candidate = request
          }
        }.buttonStyle(.borderedProminent)
      }
    }
    .padding(24).frame(width: 600)
    .confirmationDialog(
      "Apply this budget change on \(model.serverName(on: origin))?",
      isPresented: Binding(get: { candidate != nil }, set: { if !$0 { candidate = nil } }),
      presenting: candidate
    ) { request in
      Button("Apply Budget Change") {
        if model.submit(request, origin: origin) != nil { dismiss() }
      }
      Button("Cancel", role: .cancel) {}
    } message: { request in
      if case .updateUserPolicy(let update) = request {
        Text(
          "On \(model.serverName(on: origin)): disk \(Formatting.mib(draft.previous.budgetMib)) → \(Formatting.mib(update.policy.budgetMib)); minimum memory \(Formatting.mib(update.policy.minMemoryBudgetMib)), maximum memory \(Formatting.mib(update.policy.maxMemoryBudgetMib)), \(update.policy.maxCores) cores and \(update.policy.maxVms) VMs. The server rejects stale policy revisions and reductions below commitments."
        )
      }
    }
  }
}

struct AdminDevicesView: View {
  let origin: MutationOrigin?
  @Environment(AppModel.self) private var model
  @State private var enrollDraft = EnrollDeviceDraft()
  @State private var enrollIssues: [String] = []
  @State private var revokeDraft = RevokeDeviceDraft()
  @State private var revokeIssues: [String] = []
  @State private var revokeCandidate: RevokeDeviceRequest?

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 16) {
        AdminOnly {
          DashboardGate { dashboard in
            enrollForm(dashboard)
            revokeForm
          }
        }
      }
      .padding(.horizontal, 20)
      .padding(.bottom, 20)
    }
    .navigationTitle("Devices")
    .confirmationDialog(
      "Revoke this device?",
      isPresented: Binding(
        get: { revokeCandidate != nil }, set: { if !$0 { revokeCandidate = nil } }),
      presenting: revokeCandidate
    ) { request in
      Button("Revoke Device", role: .destructive) {
        if model.submit(.revokeDevice(request), origin: origin) != nil {
          revokeDraft = RevokeDeviceDraft()
        }
      }
      Button("Cancel", role: .cancel) {}
    } message: { request in
      Text("Endpoint \(request.endpointId) will no longer be able to connect.")
    }
  }

  private func enrollForm(_ dashboard: DashboardResult) -> some View {
    SectionCard(title: "Enroll Device", systemImage: "plus.viewfinder") {
      VStack(alignment: .leading, spacing: 10) {
        Text("Enter the device's public iroh endpoint ID. Never paste a private key.")
          .font(.caption)
          .foregroundStyle(.secondary)
        Form {
          Picker("User", selection: $enrollDraft.userId) {
            Text("Choose…").tag("")
            ForEach(dashboard.users.filter { $0.state == .active && $0.ready }) { user in
              Text(user.userId).tag(user.userId)
            }
          }
          TextField("Endpoint ID", text: $enrollDraft.endpointId, prompt: Text("64 hex characters"))
            .font(.body.monospaced())
            .autocorrectionDisabled()
          TextField("Label", text: $enrollDraft.label, prompt: Text("Frank laptop"))
        }
        .formStyle(.grouped)
        .frame(maxWidth: 620)
        IssueList(messages: enrollIssues)
        Button("Enroll Device") {
          switch enrollDraft.build() {
          case .failure(let failure):
            enrollIssues = failure.messages
          case .success(let request):
            enrollIssues = []
            if model.submit(request, origin: origin) != nil {
              enrollDraft = EnrollDeviceDraft()
            }
          }
        }
      }
    }
  }

  private var revokeForm: some View {
    SectionCard(title: "Revoke Device", systemImage: "minus.circle") {
      VStack(alignment: .leading, spacing: 10) {
        Form {
          TextField("Endpoint ID", text: $revokeDraft.endpointId, prompt: Text("64 hex characters"))
            .font(.body.monospaced())
            .autocorrectionDisabled()
        }
        .formStyle(.grouped)
        .frame(maxWidth: 620)
        IssueList(messages: revokeIssues)
        Button("Revoke…", role: .destructive) {
          switch revokeDraft.build() {
          case .failure(let failure):
            revokeIssues = failure.messages
          case .success(let request):
            revokeIssues = []
            if case .revokeDevice(let revoke) = request {
              revokeCandidate = revoke
            }
          }
        }
      }
    }
  }
}
