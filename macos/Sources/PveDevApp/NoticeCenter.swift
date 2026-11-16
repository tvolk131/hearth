import Foundation
import Observation
import PveDevCore

struct Notice: Identifiable, Hashable {
  enum Kind: Hashable { case success, failure }
  let id = UUID()
  let kind: Kind
  let title: String
  let detail: String
  let serverId: UUID?
  let identity: ServerIdentity?
}

/// Transient success/failure notices, each bound to the server identity it describes.
@MainActor
@Observable
final class NoticeCenter {
  private(set) var allNotices: [Notice] = []
  @ObservationIgnored private let registry: ServerRegistry

  init(registry: ServerRegistry) { self.registry = registry }

  /// Notices for still-current server identities, limited to the selected server
  /// unless a fleet page is showing.
  func visible(fleetPageActive: Bool) -> [Notice] {
    allNotices.filter { notice in
      guard let id = notice.serverId else { return true }
      guard registry.server(id)?.identity == notice.identity else { return false }
      return fleetPageActive || id == registry.selectedServerId
    }
  }

  func dismiss(_ notice: Notice) { allNotices.removeAll { $0.id == notice.id } }

  func post(
    _ kind: Notice.Kind, _ title: String, _ detail: String,
    serverId: UUID? = nil, identity: ServerIdentity? = nil, global: Bool = false
  ) {
    allNotices.append(
      Notice(
        kind: kind, title: title, detail: detail,
        serverId: global ? nil : (serverId ?? registry.selectedServerId),
        identity: global ? nil : (identity ?? registry.selectedServer?.identity)))
    if kind == .success, let notice = allNotices.last {
      Task { [weak self] in
        try? await Task.sleep(for: .seconds(8))
        self?.dismiss(notice)
      }
    }
    if allNotices.count > 32 { allNotices.removeFirst(allNotices.count - 32) }
  }
}
