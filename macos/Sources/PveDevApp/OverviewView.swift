import PveDevCore
import SwiftUI

struct DashboardGate<Content: View>: View {
  @Environment(AppModel.self) private var model
  @ViewBuilder let content: (DashboardResult) -> Content

  var body: some View {
    if let dashboard = model.dashboard {
      content(dashboard)
    } else if model.isRefreshing {
      ProgressView("Loading dashboard…")
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    } else {
      VStack(spacing: 16) {
        if let failure = model.refreshFailure {
          FailureBox(title: failure.title, detail: failure.detail)
            .frame(maxWidth: 520)
        }
        ContentUnavailableView {
          Label("No dashboard loaded", systemImage: "server.rack")
        } description: {
          Text("Loading the dashboard only reads state. Nothing is changed.")
        } actions: {
          Button("Load Dashboard") { model.refresh() }
            .keyboardShortcut(.defaultAction)
        }
      }
      .padding()
      .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
  }
}

struct StatusStrip: View {
  var global = false
  @Environment(AppModel.self) private var model

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      if model.isReadOnly {
        Label(
          "Read-only host observations. Refresh collects a new snapshot over pinned SSH. VM and account changes are unavailable.",
          systemImage: "eye"
        )
        .font(.callout).foregroundStyle(.secondary)
        .fixedSize(horizontal: false, vertical: true)
        .padding(10).frame(maxWidth: .infinity, alignment: .leading)
        .background(.teal.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
      }
      if !global, let failure = model.refreshFailure,
        model.dashboard != nil || model.observation != nil
      {
        FailureBox(
          title: "Last refresh failed: \(failure.title)",
          detail: failure.detail
            + (model.isReadOnly
              ? "\nShowing the previous observation; its capture time is unchanged."
              : "\nShowing the previous dashboard."))
      }
      if !model.isReadOnly, let issue = model.ledgerStorageIssue {
        FailureBox(
          title: "Pending request storage", detail: issue,
          systemImage: "externaldrive.badge.exclamationmark")
      }
      let unresolved = (global ? model.ledger.entries : model.pendingForCurrentProfile).filter {
        $0.phase != .rejected && !$0.isInFlight
      }
      if !unresolved.isEmpty {
        HStack(spacing: 8) {
          Image(systemName: "tray.full").foregroundStyle(.orange)
          Text(
            "\(unresolved.count) change request\(unresolved.count == 1 ? "" : "s") awaiting acknowledgement. They are never resent automatically; review them under Pending Requests."
          )
          .font(.callout)
          .fixedSize(horizontal: false, vertical: true)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.orange.opacity(0.1), in: RoundedRectangle(cornerRadius: 8))
      }
    }
  }
}
