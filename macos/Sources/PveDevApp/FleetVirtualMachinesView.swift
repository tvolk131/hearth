import PveDevCore
import SwiftUI

private struct FleetVmAction: Identifiable {
  let id = UUID()
  let row: FleetVm
  let origin: MutationOrigin
  let kind: VmActionKind
}

/// A disk or transfer sheet's VM and reviewed route. A sheet never rebinds its VM
/// to a later selection: a changed route yields no origin and the send is refused.
struct VmSheetTarget: Identifiable {
  let id = UUID()
  let vm: Vm
  let origin: MutationOrigin?
  var kind: FleetMaintenance = .grow
  func originForReview(current: MutationOrigin?) -> MutationOrigin? {
    origin == current ? origin : nil
  }
  var name: String { vm.name }
  var vmId: UInt16 { vm.vmId }
}

/// The single VM table: every trusted controller, with power actions reviewed in a
/// sheet and administrator disk/transfer actions bound to the VM's own server.
struct FleetVirtualMachinesView: View {
  @Environment(AppModel.self) private var model
  @Binding var showDeleted: Bool
  @State private var serverFilter: UUID?
  @State private var action: FleetVmAction?
  @State private var maintenance: VmSheetTarget?
  @State private var sortOrder = [
    KeyPathComparator(\FleetVm.serverName), KeyPathComparator(\FleetVm.vmId),
  ]
  private var servers: [ServerProfile] {
    model.fleetServers.filter { serverFilter == nil || $0.id == serverFilter }
  }
  private var rows: [FleetVm] {
    model.fleetRows.filter {
      (serverFilter == nil || $0.serverId == serverFilter)
        && (showDeleted || $0.vm.state != .deleted)
    }.sorted(using: sortOrder)
  }
  private var importedSections: [(server: ServerProfile, records: [ImportedVm])] {
    servers.compactMap { server in
      guard let records = model.dashboard(forServer: server.id)?.importedVms, !records.isEmpty
      else { return nil }
      return (server, records)
    }
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Picker("Server", selection: $serverFilter) {
          Text("All servers").tag(Optional<UUID>.none)
          ForEach(model.fleetServers) {
            Text($0.name).tag(Optional($0.id))
          }
        }.fixedSize()
        Spacer()
        Text("\(rows.count) virtual machine\(rows.count == 1 ? "" : "s")").foregroundStyle(
          .secondary)
      }.padding(.horizontal, 20)
      ForEach(servers) { server in
        HStack(spacing: 8) {
          if model.refreshing(server.id) { ProgressView().controlSize(.small) }
          Text("\(server.name) · \(model.freshness(server.id))").font(.caption).foregroundStyle(
            .secondary)
          if let backend = model.dashboard(forServer: server.id)?.backend {
            BackendBadge(backend: backend)
          }
          AutoRefreshPausedBanner(serverId: server.id)
          Spacer()
        }.padding(.horizontal, 20)
        if let failure = model.failure(forServer: server.id) {
          FailureBox(title: "\(server.name): \(failure.title)", detail: failure.detail).padding(
            .horizontal, 20)
        }
      }
      if rows.isEmpty {
        let loading = servers.contains { model.refreshing($0.id) }
        let complete = servers.allSatisfy {
          model.dashboard(forServer: $0.id) != nil && model.failure(forServer: $0.id) == nil
        }
        ContentUnavailableView(
          loading
            ? "Loading virtual machines"
            : (complete ? "No virtual machines" : "VM list unavailable"),
          systemImage: "desktopcomputer",
          description: Text(
            loading
              ? "Contacting the selected servers…"
              : (complete
                ? "Create a VM or refresh the selected servers."
                : "Refresh unavailable servers to load their VMs."))
        )
        .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else {
        // Keep the native table's intrinsic size from expanding the split view.
        GeometryReader { geometry in
          table(compact: geometry.size.width < 860)
        }
      }
      if !importedSections.isEmpty {
        ScrollView {
          VStack(alignment: .leading, spacing: 8) {
            ForEach(importedSections, id: \.server.id) { section in
              Text(section.server.name).font(.subheadline.weight(.semibold))
                .padding(.horizontal, 16)
              ImportedVirtualMachinesView(records: section.records, serverId: section.server.id)
            }
          }
        }
        .frame(maxHeight: 280)
      }
    }
    .navigationTitle("Virtual Machines")
    .task { model.loadFleetIfNeeded() }
    .sheet(item: $action) { FleetVmActionSheet(action: $0) }
    .sheet(item: $maintenance) { target in maintenanceSheet(target) }
  }

  private func table(compact: Bool) -> some View {
    Table(rows, sortOrder: $sortOrder) {
      TableColumn("Server", value: \.serverName) { row in
        VStack(alignment: .leading) {
          Text(row.serverName)
          if model.failure(forServer: row.serverId) != nil {
            Text("Stale").font(.caption).foregroundStyle(.orange)
          }
        }
      }.width(min: compact ? 65 : 75, ideal: compact ? 65 : 120, max: compact ? 65 : nil)
      TableColumn("Name", value: \.name) { row in
        VStack(alignment: .leading) {
          Text(row.vm.name).fontWeight(.medium)
          if !row.runningDuplicates.isEmpty {
            Text(
              compact
                ? "Other copy active"
                : "Same name active on \(row.runningDuplicates.joined(separator: ", "))"
            ).font(
              .caption
            ).foregroundStyle(.orange)
          }
        }.help(
          row.qualifiedName
            + (row.runningDuplicates.isEmpty
              ? ""
              : " · Same name active on " + row.runningDuplicates.joined(separator: ", ")))
      }.width(min: compact ? 100 : 105, ideal: compact ? 100 : 190, max: compact ? 100 : nil)
      TableColumn("ID", value: \.vmId) { Text(String($0.vm.vmId)).monospacedDigit() }.width(
        compact ? 35 : 50)
      TableColumn(DisplayVocabulary.owner) { row in
        let own = model.dashboard(forServer: row.serverId)?.identity.userId == row.vm.owner
        Text(row.vm.owner).foregroundStyle(own ? .primary : .secondary)
      }.width(min: compact ? 80 : 65, ideal: compact ? 80 : 100, max: compact ? 80 : nil)
      TableColumn("Resources") { row in
        VStack(alignment: .leading, spacing: 1) {
          Text("\(Formatting.mib(row.vm.diskMib)) disk").font(.callout)
          Text(
            "\(Formatting.mib(row.vm.minMemoryMib))–\(Formatting.mib(row.vm.memoryMib)) RAM · \(row.vm.cores) vCPU"
          ).font(.caption).foregroundStyle(.secondary)
          if model.dashboard(forServer: row.serverId)?.diskShrinks.contains(where: {
            $0.vmId == row.vmId
          }) == true {
            Text("Recovery retained").font(.caption).foregroundStyle(.secondary)
          }
        }.monospacedDigit()
      }.width(min: compact ? 125 : 115, ideal: compact ? 125 : 165, max: compact ? 125 : nil)
      TableColumn("State") { row in
        if compact {
          Text(DisplayVocabulary.vmState(row.vm.state)).font(.caption)
        } else {
          VmStateBadge(state: row.vm.state)
        }
      }.width(compact ? 65 : 85)
      TableColumn("Actions") { row in actions(row, compact: compact) }.width(
        compact ? 90 : 105)
    }
  }

  @ViewBuilder private func actions(_ row: FleetVm, compact: Bool = false) -> some View {
    if let dashboard = model.dashboard(forServer: row.serverId),
      let origin = model.origin(forServer: row.serverId)
    {
      let power = model.availableActions(for: row)
      let disk = model.maintenanceActions(for: row)
      if dashboard.operations.contains(where: { $0.vmId == row.vmId && $0.status == .blocked }) {
        Text(DisplayVocabulary.needsInvestigation)
          .help("Review Operations on this VM’s server").font(.caption)
          .foregroundStyle(.orange)
      } else if dashboard.diskShrinks.contains(where: {
        $0.owner == row.vm.owner && $0.vmId != row.vmId
      }) {
        Text("Owner disk shrink pending").font(.caption).foregroundStyle(.secondary)
          .help("Finish this owner's disk shrink on another VM first.")
      } else if model.isVmBusy(row.vmId, on: row.serverId) {
        Text("Pending").help("Review Pending Requests on this VM’s server").font(.caption)
          .foregroundStyle(.secondary)
      } else if dashboard.operations.contains(where: {
        $0.vmId == row.vmId && !$0.status.isTerminal
      }) {
        Text("In progress").help("Review Operations on this VM’s server").font(.caption)
          .foregroundStyle(.secondary)
      } else {
        Menu("Actions") {
          ForEach(power, id: \.self) { kind in
            let issue = model.actionIssue(for: row, kind: kind, origin: origin)
            Button {
              action = FleetVmAction(row: row, origin: origin, kind: kind)
            } label: {
              Text(kind == .shutdown ? "Shut Down…" : kind.rawValue.capitalized + "…")
              if let issue { Text(issue) }
            }
            .disabled(issue != nil)
            .help(issue ?? "")
          }
          if !disk.isEmpty {
            if !power.isEmpty { Divider() }
            ForEach(disk, id: \.self) { kind in
              Button {
                openMaintenance(kind, row: row)
              } label: {
                Label(maintenanceTitle(kind), systemImage: maintenanceIcon(kind))
              }
              .help(
                "Opens on \(row.serverName). Operations and Pending Requests then show that server.")
            }
          }
        }.fixedSize()
          .controlSize(compact ? .small : .regular)
          .disabled(
            (power.isEmpty && disk.isEmpty) || model.failure(forServer: row.serverId) != nil
          )
          .accessibilityLabel("Actions for \(row.vm.name) on \(row.serverName)")
          .help(menuHelp(row, empty: power.isEmpty && disk.isEmpty))
      }
    }
  }

  private func menuHelp(_ row: FleetVm, empty: Bool) -> String {
    if model.failure(forServer: row.serverId) != nil {
      return "Refresh \(row.serverName) successfully before changing this VM."
    }
    if empty {
      return model.dashboard(forServer: row.serverId).map {
        Permissions(identity: $0.identity).canManage(row.vm)
      } == true
        ? "No actions are available in this VM's current state."
        : "Only the VM's owner or an administrator can change it."
    }
    if !row.runningDuplicates.isEmpty {
      return "Starting this copy is unavailable while a same-named VM is active elsewhere."
    }
    return "Power actions are reviewed before sending. Disabled items explain why."
  }

  private func maintenanceTitle(_ kind: FleetMaintenance) -> String {
    switch kind {
    case .grow: "Expand Disk…"
    case .shrink: "Shrink Disk…"
    case .finishShrink: "Finish Shrink…"
    case .move: "Move VM…"
    }
  }
  private func maintenanceIcon(_ kind: FleetMaintenance) -> String {
    switch kind {
    case .grow: "arrow.up.left.and.arrow.down.right"
    case .shrink: "arrow.down.right.and.arrow.up.left"
    case .finishShrink: "checkmark.shield"
    case .move: "arrow.left.arrow.right"
    }
  }

  /// Disk and transfer sheets review against the selected server, so opening one
  /// selects the VM's server first; its origin is captured after that selection.
  private func openMaintenance(_ kind: FleetMaintenance, row: FleetVm) {
    model.selectServer(row.serverId)
    guard let origin = model.origin(forServer: row.serverId), origin == model.requestOrigin
    else { return }
    maintenance = VmSheetTarget(vm: row.vm, origin: origin, kind: kind)
  }

  @ViewBuilder private func maintenanceSheet(_ target: VmSheetTarget) -> some View {
    let origin = target.originForReview(current: model.requestOrigin)
    switch target.kind {
    case .grow: DiskGrowthSheet(vm: target.vm, origin: origin)
    case .shrink, .finishShrink:
      DiskShrinkSheet(
        vm: target.vm, origin: origin,
        recovery: target.origin.flatMap { model.dashboard(forServer: $0.serverId) }?.diskShrinks
          .first { $0.vmId == target.vmId })
    case .move: MigrationSheet(target: MigrationSheetTarget(vm: target.vm, origin: origin))
    }
  }
}

private struct FleetVmActionSheet: View {
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  let action: FleetVmAction
  @State private var confirmation = ""
  @State private var failure: String?
  @State private var sending = false
  @State private var attempted = false
  private var title: String {
    action.kind == .shutdown ? "Shut Down" : action.kind.rawValue.capitalized
  }
  private var issue: String? {
    model.actionIssue(for: action.row, kind: action.kind, origin: action.origin)
  }
  private var required: String? { action.row.confirmationRequired(for: action.kind) }
  var body: some View {
    VStack(alignment: .leading, spacing: 14) {
      Text("\(title) \(action.row.vm.name)?").font(.title2.weight(.semibold))
      LabeledContent("Server", value: action.row.serverName)
      LabeledContent("VM", value: "\(action.row.vm.vmId)")
      LabeledContent(DisplayVocabulary.owner, value: action.row.vm.owner)
      if action.row.existing && [.stop, .delete].contains(action.kind) {
        Text(
          "This is an existing VM with its own data and configuration. Recreating an image does not recover its current disks."
        ).foregroundStyle(.orange)
      }
      switch action.kind {
      case .delete: Text("The VM and its disks will be permanently deleted.")
      case .stop: Text("Force the guest off. Running work inside it will be interrupted.")
      case .shutdown: Text("Request a clean shutdown. This operation never forces the guest off.")
      default:
        Text(
          "Start this VM on \(action.row.serverName). Ensure another copy is not providing the same services."
        )
      }
      if let required {
        Text("Type \(required) to confirm.")
        TextField(required, text: $confirmation).textFieldStyle(.roundedBorder)
          .autocorrectionDisabled()
      }
      if let issue, !sending && !attempted { Text(issue).foregroundStyle(.orange) }
      if let failure { Text(failure).foregroundStyle(.red).textSelection(.enabled) }
      if sending {
        HStack {
          ProgressView().controlSize(.small)
          Text(
            "\(model.sendState(vmId: action.row.vmId, on: action.row.serverId)?.rawValue ?? "Queued") to \(action.row.serverName)…"
          )
        }.font(.callout)
      }
      HStack {
        Spacer()
        if sending, let entry = model.inFlightEntry(vmId: action.row.vmId, on: action.row.serverId),
          model.mutationSendStates[entry.id] == .queued
        {
          Button("Cancel Queued Request", role: .cancel) { model.cancelMutation(entry.id) }
            .keyboardShortcut(.cancelAction)
        } else {
          Button(attempted ? "Close" : "Cancel", role: .cancel) { dismiss() }.keyboardShortcut(
            .cancelAction
          ).disabled(sending)
        }
        if !attempted {
          Button(title) { send() }.disabled(
            sending || issue != nil || (required != nil && confirmation != required))
        }
      }
    }.padding(20).frame(width: 490)
      .interactiveDismissDisabled(sending)
      .onExitCommand {
        if sending {
          if let entry = model.inFlightEntry(vmId: action.row.vmId, on: action.row.serverId),
            model.mutationSendStates[entry.id] == .queued
          {
            model.cancelMutation(entry.id)
          }
        } else {
          dismiss()
        }
      }
  }
  private func send() {
    guard issue == nil else { return }
    sending = true
    attempted = true
    Task {
      defer { sending = false }
      do throws(RpcFailure) {
        _ = try await model.performMutation(
          .vmAction(VmActionRequest(requestId: UUID(), vmId: action.row.vmId, kind: action.kind)),
          origin: action.origin)
        dismiss()
      } catch {
        // Rejections are final and configuration errors explain themselves;
        // only an unknown or unsent outcome can be retried with the same ID.
        var retryable = !error.isDefinitiveRejection
        if case .configuration = error { retryable = false }
        failure =
          retryable
          ? "\(error.detail) Review Pending Requests on \(action.row.serverName); retry there using the same request ID."
          : "\(error.title): \(error.detail)"
      }
    }
  }
}
