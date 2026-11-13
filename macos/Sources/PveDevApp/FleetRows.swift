import Foundation
import PveDevCore

struct FleetVm: Identifiable {
  let serverId: UUID
  let serverName: String
  let vm: Vm
  var runningDuplicates: [String] = []
  var existing = false
  var id: String { "\(serverId)/\(vm.vmId)" }
  var name: String { vm.name }
  var vmId: UInt16 { vm.vmId }
  var qualifiedName: String { "\(serverName)/\(vm.name)" }
  func confirmationRequired(for kind: VmActionKind) -> String? {
    kind == .delete || (kind == .stop && existing) ? qualifiedName : nil
  }
  static func build(servers: [ServerProfile], dashboards: [UUID: DashboardResult]) -> [FleetVm] {
    let rows = servers.filter { !$0.isReadOnly }.flatMap { server in
      (dashboards[server.id]?.vms ?? []).map { vm in
        FleetVm(
          serverId: server.id, serverName: server.name, vm: vm,
          existing: dashboards[server.id]?.storageAllocations.contains {
            !$0.acceptsNewVms && $0.vmIds.contains(vm.vmId)
          } == true)
      }
    }
    return rows.map { row in
      var result = row
      result.runningDuplicates = rows.filter {
        $0.serverId != row.serverId && $0.vm.name == row.vm.name
          && ![.stopped, .deleted].contains($0.vm.state)
      }.map(\.serverName)
      return result
    }.sorted { ($0.serverName, $0.vmId) < ($1.serverName, $1.vmId) }
  }
}

extension AppModel {
  var fleetRows: [FleetVm] {
    FleetVm.build(
      servers: settings.servers,
      dashboards: settings.servers.reduce(into: [:]) { result, server in
        result[server.id] = dashboard(forServer: server.id)
      })
  }
  /// A fleet Start requires affirmative, recent observations from every peer.
  /// Independent controllers cannot provide a distributed exclusion lock.
  func startIssue(vm: Vm, on serverId: UUID, now: Date = Date()) -> String? {
    for peer in settings.servers where !peer.isReadOnly && peer.id != serverId {
      guard let session = sessions[peer.id], let dashboard = session.dashboard,
        session.refreshFailure == nil, !session.isRefreshing,
        let captured = session.lastRefreshedAt, now.timeIntervalSince(captured) <= 60
      else {
        return
          "Cannot verify that no same-named VM is active on \(peer.name). Refresh that server first."
      }
      if dashboard.vms.contains(where: {
        $0.name == vm.name && ![.stopped, .deleted].contains($0.state)
      }) {
        return
          "A same-named VM is active on \(peer.name). Shut it down and refresh both servers before starting this copy."
      }
    }
    return nil
  }
  func actionIssue(for row: FleetVm, kind: VmActionKind, origin: MutationOrigin) -> String? {
    guard origin == self.origin(forServer: row.serverId) else {
      return "The server profile changed. Close and reopen this action."
    }
    guard failure(forServer: row.serverId) == nil else {
      return "Refresh this server successfully before changing this VM."
    }
    guard let dashboard = dashboard(forServer: row.serverId),
      let current = dashboard.vms.first(where: { $0.vmId == row.vmId }),
      current == row.vm
    else { return "The VM changed. Close this sheet and review its current state." }
    guard !isVmBusy(row.vmId, on: row.serverId),
      !dashboard.operations.contains(where: { $0.vmId == row.vmId && !$0.status.isTerminal })
    else {
      return
        "This VM has a pending or blocked request. Review Operations and Pending Requests on its server."
    }
    guard dashboard.availableActions(for: current).contains(kind) else {
      return "This action is no longer available."
    }
    if refreshing(row.serverId) {
      return "Wait for this server's refresh to finish before changing this VM."
    }
    if kind == .start { return startIssue(vm: current, on: row.serverId) }
    return nil
  }
}
