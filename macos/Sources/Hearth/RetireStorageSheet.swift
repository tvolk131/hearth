import HearthCore
import SwiftUI

/// A deleted adopted VM's allocation that an administrator may retire.
struct RetireStorageTarget: Identifiable {
  let serverName: String
  let origin: MutationOrigin
  let vm: Vm
  let allocation: StorageAllocation
  /// The dataset's `reservation` in bytes, as last read from host inventory.
  let reservedBytes: UInt64?
  var id: String { "\(origin.serverId)/\(vm.vmId)" }
  /// Retirement is irreversible, so the VM name must be typed, as for Delete.
  var confirmationRequired: String { vm.name }
}

/// Reviews and sends `retire_storage`: removing the Proxmox storage entry and
/// the emptied ZFS dataset a deleted adopted VM left behind.
struct RetireStorageSheet: View {
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  let target: RetireStorageTarget
  @State private var confirmation = ""
  @State private var failure: String?
  @State private var sending = false
  @State private var attempted = false

  private var request: RpcRequest {
    .retireStorage(
      RetireStorageRequest(
        requestId: UUID(), vmId: target.vm.vmId, dataset: target.allocation.dataset,
        confirmRetireStorage: true))
  }
  private var issue: String? {
    guard target.origin == model.origin(forServer: target.origin.serverId),
      let dashboard = model.dashboard(forServer: target.origin.serverId)
    else { return "This server changed. Close and review it again." }
    if model.isVmBusy(target.vm.vmId, on: target.origin.serverId) {
      return "A request for this VM is still pending."
    }
    return dashboard.requestIssue(request, catalog: nil)
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 14) {
      Text("Retire storage of \(target.vm.name)?").font(.title2.weight(.semibold))
      LabeledContent("Server", value: target.serverName)
      LabeledContent("Deleted VM", value: "\(target.vm.vmId)")
      LabeledContent("Dataset") {
        Text(target.allocation.dataset).font(.body.monospaced()).textSelection(.enabled)
      }
      LabeledContent("Storage entry") {
        Text(target.allocation.storage).font(.body.monospaced()).textSelection(.enabled)
      }
      LabeledContent(
        "Reservation freed",
        value: target.reservedBytes.map { Formatting.bytes($0) } ?? "Unknown; refresh hardware")
      Text(
        "The Proxmox storage entry is removed, then the empty dataset is destroyed. The host refuses if the VM ID exists again, if the dataset has child datasets, snapshots, bookmarks or files of its own, or if anything else still uses the storage. This cannot be undone."
      )
      .fixedSize(horizontal: false, vertical: true)
      Text("Type \(target.confirmationRequired) to confirm.")
      TextField(target.confirmationRequired, text: $confirmation).textFieldStyle(.roundedBorder)
        .autocorrectionDisabled()
      if let issue, !sending && !attempted { Text(issue).foregroundStyle(.orange) }
      if let failure { Text(failure).foregroundStyle(.red).textSelection(.enabled) }
      if sending {
        HStack {
          ProgressView().controlSize(.small)
          Text("Sending to \(target.serverName)…")
        }.font(.callout)
      }
      HStack {
        Spacer()
        Button(attempted ? "Close" : "Cancel", role: .cancel) { dismiss() }
          .keyboardShortcut(.cancelAction).disabled(sending)
        if !attempted {
          Button("Retire Storage", role: .destructive) { send() }.disabled(
            sending || issue != nil || confirmation != target.confirmationRequired)
        }
      }
    }.padding(20).frame(width: 520)
      .interactiveDismissDisabled(sending)
  }

  private func send() {
    guard issue == nil, confirmation == target.confirmationRequired else { return }
    sending = true
    attempted = true
    Task {
      defer { sending = false }
      do throws(RpcFailure) {
        _ = try await model.performMutation(request, origin: target.origin)
        dismiss()
      } catch {
        var retryable = !error.isDefinitiveRejection
        if case .configuration = error { retryable = false }
        failure =
          retryable
          ? "\(error.detail) Review Pending Requests on \(target.serverName); retry there using the same request ID."
          : "\(error.title): \(error.detail)"
      }
    }
  }
}
