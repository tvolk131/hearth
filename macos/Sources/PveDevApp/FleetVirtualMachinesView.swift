import PveDevCore
import SwiftUI

private struct FleetVmAction: Identifiable {
  let id = UUID()
  let row: FleetVm
  let origin: MutationOrigin
  let kind: VmActionKind
}

struct FleetVirtualMachinesView: View {
  @Environment(AppModel.self) private var model
  @Binding var showDeleted: Bool
  @State private var serverFilter: UUID?
  @State private var action: FleetVmAction?
  @State private var sortOrder = [
    KeyPathComparator(\FleetVm.serverName), KeyPathComparator(\FleetVm.vmId),
  ]
  private var servers: [ServerProfile] {
    model.settings.servers.filter {
      !$0.isReadOnly && (serverFilter == nil || $0.id == serverFilter)
    }
  }
  private var rows: [FleetVm] {
    model.fleetRows.filter {
      (serverFilter == nil || $0.serverId == serverFilter)
        && (showDeleted || $0.vm.state != .deleted)
    }.sorted(using: sortOrder)
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        Picker("Server", selection: $serverFilter) {
          Text("All servers").tag(Optional<UUID>.none)
          ForEach(model.settings.servers.filter { !$0.isReadOnly }) {
            Text($0.name).tag(Optional($0.id))
          }
        }.fixedSize()
        Spacer()
        Text("\(rows.count) virtual machine\(rows.count == 1 ? "" : "s")").foregroundStyle(
          .secondary)
      }.padding(.horizontal, 20)
      ForEach(servers) { server in
        HStack {
          if model.refreshing(server.id) { ProgressView().controlSize(.small) }
          Text("\(server.name) · \(model.freshness(server.id))").font(.caption).foregroundStyle(
            .secondary)
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
        GeometryReader { geometry in
          let compact = geometry.size.width < 860
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
            TableColumn("Managed by") { Text($0.vm.owner) }.width(
              min: compact ? 80 : 65, ideal: compact ? 80 : 100, max: compact ? 80 : nil)
            TableColumn("Resources") { row in
              VStack(alignment: .leading, spacing: 1) {
                Text("\(Formatting.mib(row.vm.diskMib)) disk").font(.callout)
                Text("\(Formatting.mib(row.vm.memoryMib)) RAM · \(row.vm.cores) vCPU").font(
                  .caption
                ).foregroundStyle(.secondary)
              }.monospacedDigit()
            }.width(min: compact ? 125 : 115, ideal: compact ? 125 : 165, max: compact ? 125 : nil)
            TableColumn("State") { row in
              if compact {
                Text(SafeText.sanitize(row.vm.state.rawValue.capitalized, limit: 24)).font(.caption)
              } else {
                VmStateBadge(state: row.vm.state)
              }
            }.width(compact ? 65 : 85)
            TableColumn("Actions") { row in actions(row, compact: compact) }.width(
              compact ? 90 : 105)
          }
        }
      }
    }
    .navigationTitle("Virtual Machines")
    .task { model.refreshAll(onlyStale: true) }
    .sheet(item: $action) { FleetVmActionSheet(action: $0) }
  }

  @ViewBuilder private func actions(_ row: FleetVm, compact: Bool = false) -> some View {
    if let dashboard = model.dashboard(forServer: row.serverId),
      let origin = model.origin(forServer: row.serverId)
    {
      if dashboard.operations.contains(where: { $0.vmId == row.vmId && $0.status == .blocked }) {
        Text("Blocked").help("Review Operations on this VM’s server").font(.caption)
          .foregroundStyle(.orange)
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
          ForEach(dashboard.availableActions(for: row.vm), id: \.self) { kind in
            Button(kind == .shutdown ? "Shut Down…" : kind.rawValue.capitalized + "…") {
              action = FleetVmAction(row: row, origin: origin, kind: kind)
            }.disabled(model.actionIssue(for: row, kind: kind, origin: origin) != nil)
          }
        }.fixedSize()
          .controlSize(compact ? .small : .regular)
          .disabled(
            dashboard.availableActions(for: row.vm).isEmpty
              || model.failure(forServer: row.serverId) != nil
          )
          .accessibilityLabel("Actions for \(row.vm.name) on \(row.serverName)")
          .help(
            !row.runningDuplicates.isEmpty
              ? "Starting this copy is unavailable while a same-named VM is active elsewhere."
              : "Actions require a current, trusted dashboard, permission and no unfinished requests."
          )
      }
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
      LabeledContent("VM", value: "\(action.row.vm.vmId) · managed by \(action.row.vm.owner)")
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
        failure =
          "\(error.detail) Review Pending Requests on \(action.row.serverName); retry there using the same request ID."
      }
    }
  }
}
