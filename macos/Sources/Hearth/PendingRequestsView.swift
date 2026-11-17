import HearthCore
import SwiftUI

/// Every pending request from every server, labelled by server and filtered in
/// the page. Retry always goes to the request's own server.
struct PendingRequestsView: View {
  @Environment(AppModel.self) private var model
  @State private var serverFilter: UUID?
  @State private var discardTarget: PendingMutation?
  @State private var loadingImages: Set<UUID> = []
  @State private var imageStatus: [UUID: String] = [:]

  private var entries: [PendingMutation] {
    PendingFilter.entries(model.ledger.entries, serverFilter: serverFilter)
  }
  /// Managed controllers whose shown entries include a retryable create.
  private var serversNeedingImages: [ServerProfile] {
    model.fleetServers.filter { server in
      model.dashboard(forServer: server.id)?.backend == .managed
        && entries.contains {
          guard $0.serverId == server.id, model.isRoutable($0) else { return false }
          if case .createVm = $0.request { return $0.canRetry }
          return false
        }
    }
  }

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      HStack {
        ServerFilterPicker(selection: $serverFilter)
        Spacer()
        Text("\(entries.count) pending request\(entries.count == 1 ? "" : "s")")
          .foregroundStyle(.secondary)
      }.padding(.horizontal, 20)
      if entries.isEmpty {
        ContentUnavailableView(
          "No pending requests", systemImage: "tray",
          description: Text(
            serverFilter == nil || model.ledger.entries.isEmpty
              ? "Every submitted change has been acknowledged."
              : "Other servers still have pending requests. Choose All servers to see them.")
        )
        .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else {
        ScrollView {
          VStack(alignment: .leading, spacing: 16) {
            Text(
              "Change requests stay here until the server acknowledges them. Retrying reuses the original request ID and sends to the request's own server, so it can recognise a repeat instead of allocating twice. Nothing here is resent automatically, including after relaunch."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)

            ForEach(serversNeedingImages) { server in
              imageValidation(server)
            }
            ForEach(entries) { entry in
              PendingRow(entry: entry, onDiscard: { discardTarget = entry })
            }
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          .padding(.horizontal, 20)
          .padding(.bottom, 20)
        }
      }
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    .navigationTitle("Pending Requests")
    .onChange(of: model.fleetServers.map(\.id)) { _, ids in
      if let filter = serverFilter, !ids.contains(filter) { serverFilter = nil }
    }
    .confirmationDialog(
      "Discard this pending request?",
      isPresented: Binding(get: { discardTarget != nil }, set: { if !$0 { discardTarget = nil } }),
      presenting: discardTarget
    ) { entry in
      Button("Discard", role: .destructive) { model.discard(entry.id) }
      Button("Keep", role: .cancel) {}
    } message: { entry in
      Text(
        "On \(model.serverName(for: entry)): "
          + (entry.phase == .rejected
            ? "The server rejected this request. Discarding only removes it from this list."
            : "The server may already have applied this request. Refresh the dashboard first. If you discard it and submit again, a new request ID is used and the change could happen twice.")
      )
    }
  }

  private func imageValidation(_ server: ServerProfile) -> some View {
    let loading = loadingImages.contains(server.id)
    return VStack(alignment: .leading, spacing: 6) {
      Text(
        "Validate \(server.name)'s approved images before retrying a pending create after relaunch. The saved image, key, owner and request ID stay the same."
      )
      .font(.callout).foregroundStyle(.secondary)
      Button(loading ? "Loading Images…" : "Validate Images on \(server.name)") {
        let origin = model.origin(forServer: server.id)
        loadingImages.insert(server.id)
        imageStatus[server.id] = nil
        Task {
          defer { loadingImages.remove(server.id) }
          do {
            let catalog = try await model.loadImageCatalog(origin: origin)
            imageStatus[server.id] =
              catalog.images.isEmpty
              ? "No approved images are available; pending creates remain paused."
              : "Approved images loaded. Requests using an available image can be retried."
          } catch let failure as RpcFailure { imageStatus[server.id] = failure.detail } catch {
            imageStatus[server.id] = "The approved image catalog could not be loaded."
          }
        }
      }.disabled(loading)
      if let status = imageStatus[server.id] {
        Text(status).font(.caption).foregroundStyle(.secondary)
      }
    }
  }
}

struct PendingRow: View {
  @Environment(AppModel.self) private var model
  let entry: PendingMutation
  let onDiscard: () -> Void

  var body: some View {
    GroupBox {
      VStack(alignment: .leading, spacing: 8) {
        Label(model.serverName(for: entry), systemImage: "server.rack")
          .font(.caption.weight(.medium))
          .foregroundStyle(.secondary)
        HStack(alignment: .firstTextBaseline) {
          Text(entry.request.summary).font(.headline)
          if entry.request.isDestructive {
            Pill(text: "Destructive", color: .red, systemImage: "exclamationmark.triangle")
          }
          Spacer()
          phaseBadge
        }
        if let requestId = entry.wireRequestId {
          Text("request_id \(requestId)")
            .font(.caption.monospaced())
            .foregroundStyle(.secondary)
            .textSelection(.enabled)
        }
        Text(
          "Attempts: \(entry.attempts)"
            + (entry.lastAttemptAt.map { " · last \($0.relativeDescription)" } ?? "")
            + " · created \(entry.createdAt.relativeDescription)"
        )
        .font(.caption)
        .foregroundStyle(.secondary)
        if !model.isRoutable(entry) {
          Text("Profile: \(entry.profilePath)")
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        if !entry.hasServerBinding {
          Text(
            "This older request has no saved server identity. Retry is unavailable; verify its outcome before discarding it."
          )
          .font(.caption).foregroundStyle(.orange)
        } else if let identity = entry.identity {
          Text(
            "Server \(identity.serverId.prefix(12))… · environment \(identity.environment.prefix(12))…"
          )
          .font(.caption.monospaced()).foregroundStyle(.secondary)
          .help(
            "Server: \(identity.serverId)\nEnvironment: \(identity.environment)\nDevice: \(identity.deviceId)"
          )
        }
        if let title = entry.lastErrorTitle, !entry.isInFlight {
          FailureBox(title: title, detail: entry.lastErrorDetail ?? "")
        }
        HStack {
          Spacer()
          if entry.isInFlight {
            ProgressView().controlSize(.small)
            Text(model.mutationSendStates[entry.id]?.rawValue ?? "Sending")
            Button("Cancel") { model.cancelMutation(entry.id) }
              .help(
                "Cancels this queued attempt or its active client process. Already accepted server work continues; the request ID is kept."
              )
          } else {
            Button("Discard…", role: .destructive, action: onDiscard)
            if entry.canRetry {
              Button("Retry with Same Request ID") { model.retry(entry.id) }
                .disabled(!model.canRetry(entry))
                .help(
                  model.retryIssue(entry)
                    ?? "Resend exactly the same request to \(model.serverName(for: entry)) with its original device")
            }
          }
        }
        if entry.canRetry && !entry.isInFlight {
          HStack {
            Spacer()
            DisabledReason(text: model.retryIssue(entry))
          }
        }
      }
      .padding(4)
    }
  }

  @ViewBuilder
  private var phaseBadge: some View {
    switch entry.phase {
    case .inFlight:
      Pill(
        text: model.mutationSendStates[entry.id]?.rawValue ?? "Sending", color: .blue,
        systemImage: "paperplane")
    case .indeterminate:
      if entry.attempts == 0 {
        // Nothing reached the server: calm, safe to discard.
        Pill(text: "Not sent", color: .secondary, systemImage: "tray")
      } else {
        // May already have been applied.
        Pill(text: "Unacknowledged", color: .orange, systemImage: "questionmark.circle")
      }
    case .rejected:
      Pill(text: "Rejected", color: .red, systemImage: "xmark.octagon")
    }
  }
}
