import HearthCore
import SwiftUI

struct OperationRow: View {
  let operation: OperationRecord
  var serverName: String?

  var body: some View {
    HStack(alignment: .top, spacing: 10) {
      Group {
        if operation.status.isActive {
          ProgressView().controlSize(.small)
        } else if operation.status == .verified {
          Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        } else if operation.status == .cancelled {
          Image(systemName: "xmark.circle").foregroundStyle(.secondary)
        } else if operation.status == .blocked {
          Image(systemName: "exclamationmark.octagon.fill").foregroundStyle(.red)
        } else {
          Image(systemName: "questionmark.circle").foregroundStyle(.purple)
        }
      }
      .frame(width: 18)
      VStack(alignment: .leading, spacing: 3) {
        HStack(spacing: 6) {
          Text(OperationPresentation.title(for: operation.kind)).fontWeight(.medium)
          if let vmId = operation.vmId {
            Text("VM \(vmId)").foregroundStyle(.secondary)
          }
          OperationStatusBadge(status: operation.status)
          Spacer(minLength: 0)
          if let serverName {
            Label(serverName, systemImage: "server.rack")
              .font(.caption.weight(.medium))
              .foregroundStyle(.secondary)
              .lineLimit(1)
          }
          Text(operation.createdDate.relativeDescription)
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        Text("Step: \(SafeText.sanitize(operation.step, limit: 120))")
          .font(.callout)
          .foregroundStyle(.secondary)
        if let progress = operation.progress {
          OperationProgressView(progress: progress, status: operation.status)
        }
        if let error = operation.error, !error.isEmpty {
          Text(SafeText.sanitize(error, limit: 400))
            .font(.callout)
            .foregroundStyle(.red)
            .textSelection(.enabled)
            .fixedSize(horizontal: false, vertical: true)
        }
      }
    }
  }
}

/// Every controller's operations in one list, labelled by server and filtered in
/// the page. Inspect and Recover always use the operation's own server route.
struct OperationsView: View {
  @Environment(AppModel.self) private var model
  @State private var serverFilter: UUID?
  @State private var inspectionTarget: OperationInspectionTarget?
  @Binding var showCompleted: Bool

  private var servers: [ServerProfile] {
    model.fleetServers.filter { serverFilter == nil || $0.id == serverFilter }
  }
  private var rows: [FleetOperation] {
    FleetOperation.build(
      servers: model.fleetServers, dashboards: model.fleetDashboards, serverFilter: serverFilter,
      showCompleted: showCompleted)
  }

  var body: some View {
    let rows = rows
    return VStack(alignment: .leading, spacing: 12) {
      HStack {
        ServerFilterPicker(selection: $serverFilter)
        Spacer()
        Text("\(rows.count) operation\(rows.count == 1 ? "" : "s")").foregroundStyle(.secondary)
      }.padding(.horizontal, 20)
      ServerStatusLines(servers: servers)
      if rows.isEmpty {
        let loading = servers.contains { model.refreshing($0.id) }
        let complete = servers.allSatisfy {
          model.dashboard(forServer: $0.id) != nil && model.failure(forServer: $0.id) == nil
        }
        ContentUnavailableView {
          Label(
            loading
              ? "Loading operations" : (complete ? "No operations" : "Operations unavailable"),
            systemImage: "list.bullet.rectangle")
        } description: {
          Text(
            loading
              ? "Contacting the selected servers…"
              : (complete
                ? (showCompleted ? "" : "Completed operations are hidden.")
                : "Refresh unavailable servers to load their operations. Loading only reads state."))
        } actions: {
          if !loading && !complete {
            Button("Refresh Servers") {
              for server in servers { model.refresh(server.id) }
            }
          }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else {
        ScrollView {
          LazyVStack(alignment: .leading, spacing: 0) {
            ForEach(rows) { row in
              operationEntry(row)
              Divider()
            }
          }
          .frame(maxWidth: .infinity, alignment: .leading)
          .padding(.horizontal, 20)
          .padding(.bottom, 20)
        }
      }
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    .navigationTitle("Operations")
    .task { model.loadFleetIfNeeded() }
    .onChange(of: model.fleetServers.map(\.id)) { _, ids in
      if let filter = serverFilter, !ids.contains(filter) { serverFilter = nil }
    }
    .sheet(item: $inspectionTarget) { target in
      OperationInspectionSheet(target: target)
    }
  }

  private func operationEntry(_ row: FleetOperation) -> some View {
    let operation = row.operation
    return VStack(alignment: .leading, spacing: 6) {
      OperationRow(operation: operation, serverName: row.serverName)
      if canInspect(row) {
        Button("Inspect and Recover…") {
          inspectionTarget = OperationInspectionTarget(
            operation: operation, origin: model.origin(forServer: row.serverId))
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .help("Inspects this operation on \(row.serverName)")
      }
      HStack(spacing: 12) {
        Text("\(DisplayVocabulary.owner): \(operation.owner)")
        Text("Operation: \(SafeText.sanitize(operation.operationId, limit: 64))")
        Text("Request: \(SafeText.sanitize(operation.requestId, limit: 64))")
      }
      .font(.caption.monospaced())
      .foregroundStyle(.tertiary)
      .textSelection(.enabled)
    }
    .padding(.vertical, 10)
  }

  private func canInspect(_ row: FleetOperation) -> Bool {
    guard row.operation.status == .blocked, model.isAdmin(onServer: row.serverId),
      let dashboard = model.dashboard(forServer: row.serverId)
    else { return false }
    return dashboard.supports("inspect_operation")
  }
}

/// The in-page server filter used by pages that list every controller.
struct ServerFilterPicker: View {
  @Environment(AppModel.self) private var model
  @Binding var selection: UUID?

  var body: some View {
    Picker("Server", selection: $selection) {
      Text("All servers").tag(UUID?.none)
      ForEach(model.fleetServers) { Text($0.name).tag(UUID?.some($0.id)) }
    }
    .fixedSize()
    .accessibilityIdentifier("server-filter")
  }
}

/// One line per listed controller: freshness, backend, paused auto-refresh and
/// the last refresh failure.
struct ServerStatusLines: View {
  @Environment(AppModel.self) private var model
  let servers: [ServerProfile]

  var body: some View {
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
  }
}

private struct OperationProgressView: View {
    let progress: OperationProgress
    let status: OperationStatus

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let fraction = progress.fractionCompleted {
                HStack(spacing: 8) {
                    ProgressView(value: fraction).frame(maxWidth: 180)
                    Text("\(progress.completedSteps) of \(progress.totalSteps) stages completed")
                        .font(.caption).foregroundStyle(.secondary)
                }
                .accessibilityElement(children: .combine)
            }
            if let detail = progress.detail, !detail.isEmpty {
                Text(SafeText.sanitize(detail, limit: 240)).font(.callout).foregroundStyle(.secondary)
            }
            TimelineView(.periodic(from: .now, by: 5)) { context in
                HStack(spacing: 12) {
                    if let seconds = progress.elapsedSeconds(now: context.date) {
                        Text("\(status.isActive ? "Elapsed" : "Duration"): \(OperationPresentation.duration(seconds))")
                    }
                    if progress.updatedAt > 0 && progress.updatedAt <= UInt64(max(0, context.date.timeIntervalSince1970)) {
                        Text("Updated \(Date(timeIntervalSince1970: TimeInterval(progress.updatedAt)).relativeDescription)")
                    }
                }
                .font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
        }
    }
}
