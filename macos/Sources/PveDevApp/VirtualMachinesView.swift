import PveDevCore
import SwiftUI

struct LegacyVmTarget: Identifiable {
  let id = UUID()
  let vm: Vm
  let origin: MutationOrigin?
  func originForReview(current: MutationOrigin?) -> MutationOrigin? {
    origin == current ? origin : nil
  }
  var name: String { vm.name }
  var vmId: UInt16 { vm.vmId }
}

/// Legacy lab/trial UI: disk maintenance and enrollment are capability-gated.
struct LegacyVirtualMachinesView: View {
  @Environment(AppModel.self) private var model
  @Binding var showDeleted: Bool
  @Binding var showEnroll: Bool
  let origin: MutationOrigin?
  @State private var stopTarget: LegacyVmTarget?
  @State private var shutdownTarget: LegacyVmTarget?
  @State private var deleteTarget: LegacyVmTarget?
  @State private var shrinkTarget: LegacyVmTarget?
  @State private var growthTarget: LegacyVmTarget?
  @State private var migrationTarget: MigrationSheetTarget?
  @State private var sortOrder = [KeyPathComparator(\Vm.vmId)]

  var body: some View {
    // Keep the native table's intrinsic size from expanding the split view
    // beyond the window and moving the sidebar offscreen.
    GeometryReader { geometry in
      DashboardGate { dashboard in
        VStack(alignment: .leading, spacing: 12) {
          table(dashboard)
          if !dashboard.importedVms.isEmpty {
            ImportedVirtualMachinesView(
              records: dashboard.importedVms, serverId: model.selectedServerId
            )
            .frame(maxHeight: 280)
          }
        }
      }
      .frame(width: geometry.size.width, height: geometry.size.height, alignment: .topLeading)
    }
    .navigationTitle("Virtual Machines")
    .sheet(isPresented: $showEnroll) {
      EnrollmentSheet(origin: origin)
    }
    .sheet(item: $deleteTarget) { vm in
      DeleteVmSheet(vm: vm.vm, origin: vm.originForReview(current: model.requestOrigin))
    }
    .sheet(item: $migrationTarget) { target in
      MigrationSheet(target: target)
    }
    .sheet(item: $growthTarget) { vm in
      DiskGrowthSheet(vm: vm.vm, origin: vm.originForReview(current: model.requestOrigin))
    }
    .sheet(item: $shrinkTarget) { vm in
      DiskShrinkSheet(
        vm: vm.vm, origin: vm.originForReview(current: model.requestOrigin),
        recovery: model.dashboard?.diskShrinks.first { $0.vmId == vm.vmId })
    }
    .confirmationDialog(
      "Shut down \(shutdownTarget?.name ?? "VM")?",
      isPresented: Binding(
        get: { shutdownTarget != nil }, set: { if !$0 { shutdownTarget = nil } }),
      presenting: shutdownTarget
    ) { vm in
      Button("Shut Down \(vm.name)") {
        model.submit(
          .vmAction(VmActionRequest(requestId: UUID(), vmId: vm.vmId, kind: .shutdown)),
          origin: vm.originForReview(current: model.requestOrigin))
      }
      Button("Cancel", role: .cancel) {}
    } message: { _ in
      Text(
        "Ask the guest to shut down cleanly. The operation waits for the guest and never forces it off."
      )
    }
    .confirmationDialog(
      "Stop \(stopTarget?.name ?? "VM")?",
      isPresented: Binding(get: { stopTarget != nil }, set: { if !$0 { stopTarget = nil } }),
      presenting: stopTarget
    ) { vm in
      Button("Stop \(vm.name)") {
        model.submit(
          .vmAction(VmActionRequest(requestId: UUID(), vmId: vm.vmId, kind: .stop)),
          origin: vm.originForReview(current: model.requestOrigin))
      }
      Button("Cancel", role: .cancel) {}
    } message: { _ in
      Text("Running work inside the guest will be interrupted.")
    }
  }

  private func table(_ dashboard: DashboardResult) -> some View {
    let rows = dashboard.vms
      .filter { vm in !dashboard.importedVms.contains(where: { $0.vm.vmId == vm.vmId }) }
      .filter { showDeleted || $0.state != .deleted }
      .sorted(using: sortOrder)
    let permissions = model.permissions
    return Table(rows, sortOrder: $sortOrder) {
      TableColumn("Name", value: \.name) { vm in
        Text(vm.name).fontWeight(.medium)
      }
      TableColumn("ID", value: \.vmId) { vm in
        Text(String(vm.vmId)).monospacedDigit()
      }
      .width(min: 50, ideal: 60)
      TableColumn("Owner", value: \.owner) { vm in
        Text(vm.owner).foregroundStyle(vm.owner == permissions.userId ? .primary : .secondary)
      }
      TableColumn("Disk", value: \.diskMib) { vm in
        VStack(alignment: .leading) {
          Text(Formatting.mib(vm.diskMib)).monospacedDigit()
          if dashboard.diskShrinks.contains(where: { $0.vmId == vm.vmId }) {
            Text("Recovery retained").font(.caption).foregroundStyle(.secondary)
          }
        }
      }
      TableColumn("Memory") { vm in
        Text("\(Formatting.mib(vm.minMemoryMib))–\(Formatting.mib(vm.memoryMib))").monospacedDigit()
      }
      TableColumn("Cores", value: \.cores) { vm in
        Text(String(vm.cores)).monospacedDigit()
      }
      .width(min: 40, ideal: 50)
      TableColumn("State") { vm in
        VmStateBadge(state: vm.state)
      }
      TableColumn("Actions") { vm in
        actions(for: vm, permissions: permissions)
      }
      .width(min: 150, ideal: 190)
    }
  }

  @ViewBuilder
  private func actions(for vm: Vm, permissions: Permissions) -> some View {
    if model.dashboard?.importedVms.contains(where: { $0.vm.vmId == vm.vmId }) == true {
      Text("Imported read-only").font(.caption).foregroundStyle(.secondary)
    } else if model.dashboard?.operations.contains(where: {
      $0.vmId == vm.vmId && $0.status == .blocked
    }) == true {
      Text("Needs investigation").font(.caption).foregroundStyle(.secondary)
    } else if model.dashboard?.diskShrinks.contains(where: {
      $0.owner == vm.owner && $0.vmId != vm.vmId
    }) == true {
      Text("Owner disk shrink pending").font(.caption).foregroundStyle(.secondary)
    } else if model.isVmBusy(vm.vmId) {
      HStack(spacing: 6) {
        ProgressView().controlSize(.small)
        Text("Request pending").font(.caption).foregroundStyle(.secondary)
      }
    } else {
      let available = model.dashboard?.availableActions(for: vm) ?? []
      HStack(spacing: 6) {
        if available.contains(.shutdown)
          && model.dashboard?.supports("shutdown", legacyLabFallback: true) == true
        {
          Button {
            shutdownTarget = LegacyVmTarget(vm: vm, origin: origin)
          } label: {
            Label("Shut Down", systemImage: "power")
          }
          .help("Shut down the guest cleanly before shrinking its disk")
        }
        if available.contains(.start) {
          Button {
            model.submit(
              .vmAction(VmActionRequest(requestId: UUID(), vmId: vm.vmId, kind: .start)),
              origin: origin)
          } label: {
            Label("Start", systemImage: "play.fill")
          }
        }
        if available.contains(.stop) {
          Button {
            stopTarget = LegacyVmTarget(vm: vm, origin: origin)
          } label: {
            Label("Stop", systemImage: "stop.fill")
          }
        }
        if available.contains(.delete)
          && !(model.dashboard?.diskShrinks.contains { $0.owner == vm.owner } ?? false)
        {
          Button(role: .destructive) {
            deleteTarget = LegacyVmTarget(vm: vm, origin: origin)
          } label: {
            Label("Delete", systemImage: "trash")
          }
        }
        if permissions.isAdmin && !model.isReadOnly && vm.state == .stopped {
          let ownerHasRecovery =
            model.dashboard?.diskShrinks.contains { $0.owner == vm.owner } ?? false
          let recovery = model.dashboard?.diskShrinks.first { $0.vmId == vm.vmId }
          let canGrow = !ownerHasRecovery && model.dashboard?.supports("grow_vm") == true
          let canMove = !ownerHasRecovery && model.dashboard?.supports("migration") == true
          let canShrink =
            model.dashboard?.supports("shrink_vm", legacyLabFallback: true) == true
            && (recovery?.prepared == true || (!ownerHasRecovery && vm.diskMib > 2048))
          if canGrow || canMove || canShrink {
            Menu {
              if canGrow {
                Button {
                  growthTarget = LegacyVmTarget(vm: vm, origin: origin)
                } label: {
                  Label("Expand Disk…", systemImage: "arrow.up.left.and.arrow.down.right")
                }
              }
              if canShrink {
                Button {
                  shrinkTarget = LegacyVmTarget(vm: vm, origin: origin)
                } label: {
                  Label(
                    recovery == nil ? "Shrink Disk…" : "Finish Shrink…",
                    systemImage: recovery == nil
                      ? "arrow.down.right.and.arrow.up.left" : "checkmark.shield")
                }
              }
              if canMove {
                Button {
                  migrationTarget = MigrationSheetTarget(vm: vm, origin: origin)
                } label: {
                  Label("Move VM…", systemImage: "arrow.left.arrow.right")
                }
              }
            } label: {
              Label("More Actions", systemImage: "ellipsis")
            }
            .help("Disk resizing and server transfer actions")
          }
        }
      }
      .labelStyle(.iconOnly)
      .buttonStyle(.bordered)
      .controlSize(.small)
      .accessibilityElement(children: .contain)
    }
  }
}

struct CreateVmSheet: View {
  let origin: MutationOrigin?
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  @State private var draft = CreateVmDraft()
  @State private var issues: [String] = []
  @State private var catalog: ImageCatalog?
  @State private var loadingCatalog = false
  @State private var catalogFailure: String?

  private var dashboard: DashboardResult? {
    origin.flatMap { model.dashboard(forServer: $0.serverId) }
  }
  private var permissions: Permissions { Permissions(identity: dashboard?.identity) }
  private var managed: Bool { dashboard?.backend == .managed }
  private var selectedImage: ApprovedImage? {
    catalog?.images.first { $0.imageSha256 == draft.imageSha256 }
  }

  var body: some View {
    let budget = dashboard?.budget
    let targetOwner = draft.owner.isEmpty ? permissions.userId ?? "" : draft.owner
    let allocation = dashboard?.storageAllocations.first {
      $0.owner == targetOwner && $0.acceptsNewVms
    }
    let ownerHasRecovery =
      dashboard?.diskShrinks.contains {
        $0.owner == (draft.owner.isEmpty ? permissions.userId ?? "" : draft.owner)
      } == true
    VStack(alignment: .leading, spacing: 14) {
      Text("New Virtual Machine").font(.title2.weight(.semibold))
      LabeledContent("Server", value: model.serverName(on: origin))
      Text(
        managed
          ? "Create a bootable guest from this server’s approved image catalog."
          : "The VM is created with a blank root disk in the selected controller’s lab storage."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      Form {
        TextField("Name", text: $draft.name, prompt: Text("dev-frank"))
        if managed {
          Picker("Boot image", selection: $draft.imageSha256) {
            Text("Choose an approved image").tag("")
            ForEach(catalog?.images ?? []) { image in
              Text(image.displayLabel).tag(image.imageSha256)
            }
          }
          if let image = selectedImage {
            Text(image.accessLabel)
            Text("Minimum disk: \(image.minDiskMib) MiB")
              .foregroundStyle(.secondary)
          }
          TextField(
            "Ed25519 public key", text: $draft.sshPublicKey, prompt: Text("ssh-ed25519 AAAA…")
          )
          .autocorrectionDisabled()
          .accessibilityIdentifier("guest-ssh-public-key")
        }
        TextField("Disk (MiB)", text: $draft.diskMib)
        TextField("Memory (MiB)", text: $draft.memoryMib)
        TextField("Minimum memory (MiB)", text: $draft.minMemoryMib)
        TextField("Cores", text: $draft.cores)
        if permissions.canChooseVmOwner {
          Picker("Owner", selection: $draft.owner) {
            Text("Myself").tag("")
            ForEach((dashboard?.users ?? []).filter { $0.state == .active && $0.ready }) { user in
              if user.userId != permissions.userId {
                Text(user.userId).tag(user.userId)
              }
            }
          }
        }
      }
      .formStyle(.grouped)
      if let allocation {
        Text("Allocation: \(allocation.dataset) · \(Formatting.mib(allocation.budgetMib)) budget")
          .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
      }
      if managed {
        HStack {
          if loadingCatalog {
            ProgressView().controlSize(.small)
            Text("Loading approved images…")
          } else if let catalogFailure {
            Text(catalogFailure).foregroundStyle(.red)
          } else if catalog?.images.isEmpty == true {
            Text(
              "No approved boot images are available on this server. Ask its administrator to configure an image."
            )
          }
          Button("Reload Images") { Task { await loadCatalog() } }.disabled(loadingCatalog)
        }.font(.caption)
      }
      if let budget, draft.owner.isEmpty {
        Text(
          "Your disk budget: \(Formatting.mib(budget.remainingMib)) remaining of \(Formatting.mib(budget.budgetMib))."
        )
        .font(.caption)
        .foregroundStyle(.secondary)
      }
      IssueList(messages: issues)
      if ownerHasRecovery {
        Text("Finish this owner's disk shrink before creating another VM.").font(.callout)
          .foregroundStyle(.secondary)
      }
      HStack {
        Spacer()
        Button("Cancel", role: .cancel) { dismiss() }
          .keyboardShortcut(.cancelAction)
        Button("Create VM") { create() }
          .keyboardShortcut(.defaultAction)
          .disabled(
            ownerHasRecovery || origin != origin.flatMap { model.origin(forServer: $0.serverId) }
              || (managed && (loadingCatalog || selectedImage == nil || catalogFailure != nil)))
      }
    }
    .padding(20)
    .frame(width: 540)
    .task {
      if managed {
        draft.diskMib = "8192"
        draft.memoryMib = "1024"
        draft.minMemoryMib = "512"
        await loadCatalog()
      }
    }
  }

  private func loadCatalog() async {
    loadingCatalog = true
    catalogFailure = nil
    catalog = nil
    draft.imageSha256 = ""
    defer { loadingCatalog = false }
    do { catalog = try await model.loadImageCatalog(origin: origin) } catch {
      catalogFailure = error.detail
    }
  }

  private func create() {
    switch draft.build(
      requestId: UUID(), allowOwner: permissions.canChooseVmOwner, requireGuest: managed,
      catalog: catalog)
    {
    case .failure(let failure):
      issues = failure.messages
    case .success(let request):
      if model.submit(request, origin: origin) != nil {
        dismiss()
      }
    }
  }
}

struct DeleteVmSheet: View {
  // A stale sheet can never target a replacement profile with the same VMID.
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  let vm: Vm
  let origin: MutationOrigin?
  @State private var confirmation = ""

  var body: some View {
    VStack(alignment: .leading, spacing: 14) {
      Label("Delete \(vm.name)?", systemImage: "trash")
        .font(.title2.weight(.semibold))
        .foregroundStyle(.red)
      Text(
        "VM \(vm.vmId) owned by \(vm.owner) and its disk will be destroyed on this server. This cannot be undone."
      )
      .fixedSize(horizontal: false, vertical: true)
      Text("Type the VM name to confirm.").font(.callout).foregroundStyle(.secondary)
      TextField(vm.name, text: $confirmation)
        .textFieldStyle(.roundedBorder)
        .autocorrectionDisabled()
      HStack {
        Spacer()
        Button("Cancel", role: .cancel) { dismiss() }
          .keyboardShortcut(.cancelAction)
        Button("Delete VM", role: .destructive) {
          model.submit(
            .vmAction(VmActionRequest(requestId: UUID(), vmId: vm.vmId, kind: .delete)),
            origin: origin)
          dismiss()
        }
        .disabled(confirmation != vm.name)
      }
    }
    .padding(20)
    .frame(width: 420)
  }
}
