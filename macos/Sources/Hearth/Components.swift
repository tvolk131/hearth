import HearthCore
import SwiftUI

struct Pill: View {
  let text: String
  let color: Color
  var systemImage: String?

  var body: some View {
    Label {
      Text(text)
    } icon: {
      if let systemImage {
        Image(systemName: systemImage)
      }
    }
    .labelStyle(.titleAndIcon)
    .font(.caption.weight(.medium))
    .padding(.horizontal, 7)
    .padding(.vertical, 2)
    .foregroundStyle(color)
    .background(color.opacity(0.14), in: Capsule())
  }
}

struct VmStateBadge: View {
  let state: VmState

  var body: some View {
    let text = DisplayVocabulary.vmState(state)
    switch state {
    case .running: Pill(text: text, color: .green, systemImage: "play.fill")
    case .stopped: Pill(text: text, color: .secondary, systemImage: "stop.fill")
    case .creating: Pill(text: text, color: .blue, systemImage: "hourglass")
    case .deleting: Pill(text: text, color: .orange, systemImage: "hourglass")
    case .deleted: Pill(text: text, color: .secondary, systemImage: "trash")
    case .blocked: Pill(text: text, color: .red, systemImage: "exclamationmark.octagon.fill")
    default: Pill(text: text, color: .purple, systemImage: "questionmark")
    }
  }
}

struct OperationStatusBadge: View {
  let status: OperationStatus

  var body: some View {
    let text = DisplayVocabulary.operationStatus(status)
    switch status {
    case .queued: Pill(text: text, color: .secondary, systemImage: "clock")
    case .running: Pill(text: text, color: .blue, systemImage: "arrow.triangle.2.circlepath")
    case .verified: Pill(text: text, color: .green, systemImage: "checkmark.seal.fill")
    case .blocked: Pill(text: text, color: .red, systemImage: "exclamationmark.octagon.fill")
    case .cancelled: Pill(text: text, color: .secondary, systemImage: "xmark.circle")
    case .failed: Pill(text: text, color: .orange, systemImage: "xmark.octagon")
    default: Pill(text: text, color: .purple, systemImage: "questionmark")
    }
  }
}

struct RoleBadge: View {
  let role: UserRole

  var body: some View {
    if role.isAdmin {
      Pill(text: "Admin", color: .orange, systemImage: "shield.lefthalf.filled")
    } else if role == .member {
      Pill(text: "Member", color: .blue, systemImage: "person.fill")
    } else {
      Pill(
        text: SafeText.sanitize(role.rawValue, limit: 24), color: .purple,
        systemImage: "questionmark")
    }
  }
}

/// Lab and simulated controllers use disposable lab storage; a physical trial
/// acts on real hardware and is deliberately loud (orange, filled).
struct BackendBadge: View {
  let backend: BackendKind

  var body: some View {
    let text = DisplayVocabulary.backend(backend)
    switch backend {
    case .lab: Pill(text: text, color: .blue, systemImage: "flask")
    case .simulated: Pill(text: text, color: .teal, systemImage: "cpu")
    case .managed: Pill(text: text, color: .green, systemImage: "server.rack")
    case .physicalTrial:
      Label(text, systemImage: "exclamationmark.triangle.fill")
        .labelStyle(.titleAndIcon)
        .font(.caption.weight(.semibold))
        .padding(.horizontal, 7)
        .padding(.vertical, 2)
        .foregroundStyle(.white)
        .background(.orange, in: Capsule())
    default: Pill(text: text, color: .purple, systemImage: "questionmark")
    }
  }
}

/// A disabled control's reason, shown inline beneath it.
struct DisabledReason: View {
  let text: String?

  var body: some View {
    if let text, !text.isEmpty {
      Label(text, systemImage: "info.circle")
        .font(.caption)
        .foregroundStyle(.secondary)
        .fixedSize(horizontal: false, vertical: true)
    }
  }
}

/// Shown when a failed background read paused a server's auto-refresh.
struct AutoRefreshPausedBanner: View {
  @Environment(AppModel.self) private var model
  let serverId: UUID
  var serverName: String?

  var body: some View {
    if model.autoRefreshPaused(serverId) {
      HStack(spacing: 6) {
        Image(systemName: "pause.circle.fill").foregroundStyle(.orange)
        Text((serverName.map { "\($0): " } ?? "") + "Auto-refresh paused")
        Text("·").foregroundStyle(.secondary)
        Button("Retry") { model.retryAutoRefresh(serverId) }
          .buttonStyle(.link)
          .disabled(model.refreshing(serverId))
      }
      .font(.caption)
      .help("The last background refresh failed, so automatic refreshes stopped. Retry reads the dashboard once and resumes auto-refresh if it succeeds.")
      .accessibilityElement(children: .combine)
    }
  }
}

struct FailureBox: View {
  let title: String
  let detail: String
  var systemImage = "exclamationmark.triangle.fill"

  var body: some View {
    HStack(alignment: .top, spacing: 10) {
      Image(systemName: systemImage)
        .foregroundStyle(.red)
      VStack(alignment: .leading, spacing: 4) {
        Text(title).font(.headline)
        Text(detail)
          .font(.callout)
          .foregroundStyle(.secondary)
          .textSelection(.enabled)
          .fixedSize(horizontal: false, vertical: true)
      }
      Spacer(minLength: 0)
    }
    .padding(12)
    .background(.red.opacity(0.07), in: RoundedRectangle(cornerRadius: 8))
  }
}

struct IssueList: View {
  let messages: [String]

  var body: some View {
    if !messages.isEmpty {
      VStack(alignment: .leading, spacing: 3) {
        ForEach(messages, id: \.self) { message in
          Label(message, systemImage: "exclamationmark.circle")
            .font(.callout)
            .foregroundStyle(.red)
        }
      }
    }
  }
}

struct NoticeStack: View {
  @Environment(AppModel.self) private var model

  var body: some View {
    VStack(alignment: .trailing, spacing: 8) {
      ForEach(model.notices.suffix(3)) { notice in
        HStack(alignment: .top, spacing: 10) {
          Image(
            systemName: notice.kind == .success
              ? "checkmark.circle.fill" : "exclamationmark.triangle.fill"
          )
          .foregroundStyle(notice.kind == .success ? .green : .red)
          VStack(alignment: .leading, spacing: 2) {
            Text(notice.title).font(.callout.weight(.semibold))
            Text(notice.detail)
              .font(.caption)
              .foregroundStyle(.secondary)
              .lineLimit(6)
              .textSelection(.enabled)
          }
          Spacer(minLength: 0)
          Button {
            model.dismissNotice(notice)
          } label: {
            Image(systemName: "xmark")
          }
          .buttonStyle(.borderless)
          .help("Dismiss")
        }
        .padding(10)
        .frame(width: 360)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        .shadow(radius: 4, y: 2)
      }
    }
    .padding()
    .overlay(alignment: .topTrailing) {
      if model.notices.count > 3 {
        Button("Dismiss \(model.notices.count - 3) older notices") {
          for notice in model.notices.dropLast(3) { model.dismissNotice(notice) }
        }.font(.caption).padding(.trailing, 16).offset(y: -8)
      }
    }
  }
}

struct SectionCard<Content: View>: View {
  let title: String
  var systemImage: String?
  @ViewBuilder let content: Content

  var body: some View {
    GroupBox {
      content
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(4)
    } label: {
      if let systemImage {
        Label(title, systemImage: systemImage).font(.headline)
      } else {
        Text(title).font(.headline)
      }
    }
  }
}

struct NotConfiguredView: View {
  @Environment(\.openSettings) private var openSettings
  let issues: [String]

  var body: some View {
    ContentUnavailableView {
      Label("Connect to hearth", systemImage: "gearshape.2")
    } description: {
      VStack(spacing: 6) {
        Text("Choose the hearth executable and your profile JSON in Settings.")
        ForEach(issues, id: \.self) { issue in
          Text(issue).font(.caption).foregroundStyle(.secondary)
        }
      }
    } actions: {
      Button("Open Settings…") { openSettings() }
    }
  }
}

extension Date {
  var relativeDescription: String {
    RelativeDateTimeFormatter().localizedString(for: self, relativeTo: Date())
  }
}
