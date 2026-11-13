import PveDevCore
import SwiftUI

struct PendingRequestsView: View {
  @Environment(AppModel.self) private var model
  @State private var discardTarget: PendingMutation?
  @State private var loadingImages = false
  @State private var imageStatus: String?

  var body: some View {
    Group {
      if model.pendingForCurrentProfile.isEmpty && model.pendingForOtherProfiles.isEmpty {
        ContentUnavailableView(
          "No pending requests", systemImage: "tray",
          description: Text("Every submitted change has been acknowledged.")
        )
        .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else {
        ScrollView {
          VStack(alignment: .leading, spacing: 16) {
            Text(
              "Change requests stay here until the server acknowledges them. Retrying reuses the original request ID, so the server can recognise a repeat instead of allocating twice. Nothing here is resent automatically, including after relaunch."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)

            let current = model.pendingForCurrentProfile
            if model.dashboard?.backend == .managed
              && current.contains(where: {
                if case .createVm = $0.request { return $0.canRetry }
                return false
              })
            {
              VStack(alignment: .leading, spacing: 6) {
                Text(
                  "Validate this controller’s approved images before retrying a pending create after relaunch. The saved image, key, owner and request ID stay the same."
                )
                .font(.callout).foregroundStyle(.secondary)
                Button(loadingImages ? "Loading Images…" : "Validate Images for Pending Creates") {
                  let origin = model.requestOrigin
                  loadingImages = true
                  imageStatus = nil
                  Task {
                    defer { loadingImages = false }
                    do {
                      let catalog = try await model.loadImageCatalog(origin: origin)
                      imageStatus =
                        catalog.images.isEmpty
                        ? "No approved images are available; pending creates remain paused."
                        : "Approved images loaded. Requests using an available image can be retried."
                    } catch let failure as RpcFailure { imageStatus = failure.detail } catch {
                      imageStatus = "The approved image catalog could not be loaded."
                    }
                  }
                }.disabled(loadingImages)
                if let imageStatus { Text(imageStatus).font(.caption).foregroundStyle(.secondary) }
              }
            }
            if current.isEmpty {
              ContentUnavailableView(
                "No pending requests", systemImage: "tray",
                description: Text("Every submitted change has been acknowledged.")
              )
              .frame(maxWidth: .infinity)
            } else {
              ForEach(current) { entry in
                PendingRow(
                  entry: entry, isCurrentProfile: true, onDiscard: { discardTarget = entry })
              }
            }

            let others = model.pendingForOtherProfiles
            if !others.isEmpty {
              Text("Other servers and older requests").font(.headline).padding(.top, 8)
              ForEach(others) { entry in
                PendingRow(
                  entry: entry, isCurrentProfile: false, onDiscard: { discardTarget = entry })
              }
            }
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          .padding(.horizontal, 20)
          .padding(.bottom, 20)
        }
      }
    }
    .navigationTitle("Pending Requests")
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
}

struct PendingRow: View {
  @Environment(AppModel.self) private var model
  let entry: PendingMutation
  let isCurrentProfile: Bool
  let onDiscard: () -> Void

  var body: some View {
    GroupBox {
      VStack(alignment: .leading, spacing: 8) {
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
        if !isCurrentProfile {
          Text("\(model.serverName(for: entry)) · Profile: \(entry.profilePath)")
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
                  isCurrentProfile
                    ? "Resend exactly the same request to its original server and device"
                    : "Select this request's original server and identity to retry it")
            }
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
      Pill(
        text: entry.attempts == 0 ? "Not sent" : "Unacknowledged", color: .orange,
        systemImage: "questionmark.circle")
    case .rejected:
      Pill(text: "Rejected", color: .red, systemImage: "xmark.octagon")
    }
  }
}
