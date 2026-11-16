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
  /// Start is the only action sent straight from the menu, and only while no
  /// same-named VM is running elsewhere: two copies would serve the same things.
  func sendsWithoutReview(_ kind: VmActionKind) -> Bool {
    kind == .start && runningDuplicates.isEmpty
  }
  static func build(servers: [ServerProfile], dashboards: [UUID: DashboardResult]) -> [FleetVm] {
    let rows = servers.filter { !$0.isReadOnly }.flatMap { server in
      // Imported records are listed separately, outside the actionable table.
      let imported = Set(dashboards[server.id]?.importedVms.map(\.vm.vmId) ?? [])
      return (dashboards[server.id]?.vms ?? []).filter { !imported.contains($0.vmId) }.map { vm in
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
        "This VM has a pending request or an operation that needs investigation. Review Operations and Pending Requests on its server."
    }
    guard dashboard.availableActions(for: current).contains(kind) else {
      return "This action is no longer available."
    }
    if kind == .delete && dashboard.diskShrinks.contains(where: { $0.owner == current.owner }) {
      return "Finish this owner's disk shrink before deleting a VM."
    }
    if kind == .shutdown && !dashboard.supports("shutdown", legacyLabFallback: true) {
      return "This server has not enabled clean shutdown."
    }
    if refreshing(row.serverId) {
      return "Wait for this server's refresh to finish before changing this VM."
    }
    if kind == .start { return startIssue(vm: current, on: row.serverId) }
    return nil
  }

  /// Power actions offered for a row. A missing clean-shutdown capability and a
  /// pending owner disk shrink hide the action instead of failing on send.
  func availableActions(for row: FleetVm) -> [VmActionKind] {
    guard let dashboard = dashboard(forServer: row.serverId) else { return [] }
    return dashboard.availableActions(for: row.vm).filter { kind in
      switch kind {
      case .shutdown: dashboard.supports("shutdown", legacyLabFallback: true)
      case .delete: !dashboard.diskShrinks.contains { $0.owner == row.vm.owner }
      default: true
      }
    }
  }

  /// Administrator disk and transfer actions for a stopped VM on its own server.
  func maintenanceActions(for row: FleetVm) -> [FleetMaintenance] {
    guard let dashboard = dashboard(forServer: row.serverId),
      Permissions(identity: dashboard.identity).isAdmin, row.vm.state == .stopped,
      failure(forServer: row.serverId) == nil
    else { return [] }
    let ownerHasRecovery = dashboard.diskShrinks.contains { $0.owner == row.vm.owner }
    let recovery = dashboard.diskShrinks.first { $0.vmId == row.vmId }
    var actions: [FleetMaintenance] = []
    if !ownerHasRecovery && dashboard.supports("grow_vm") { actions.append(.grow) }
    if dashboard.supports("shrink_vm", legacyLabFallback: true)
      && (recovery?.prepared == true || (!ownerHasRecovery && row.vm.diskMib > 2048))
    {
      actions.append(recovery == nil ? .shrink : .finishShrink)
    }
    if !ownerHasRecovery && dashboard.supports("migration") { actions.append(.move) }
    return actions
  }

  /// Why the New VM menu cannot create on this server, or nil when it can.
  func createVmIssue(forServer id: UUID) -> String? {
    guard let server = settings.servers.first(where: { $0.id == id }), !server.isReadOnly else {
      return "This server is not a controller."
    }
    guard server.isTrusted else { return "Inspect and trust this profile in Settings first." }
    guard let dashboard = dashboard(forServer: id) else {
      if refreshing(id) { return "Loading this server's dashboard…" }
      return failure(forServer: id) == nil
        ? "Load this server's dashboard first."
        : "The dashboard failed to load. Refresh this server."
    }
    if let expected = server.expectedBackend, dashboard.backend != expected {
      return "The controller backend differs from this profile's expected backend."
    }
    guard dashboard.backend.isRecognized else { return "This controller backend is unsupported." }
    let permissions = Permissions(identity: dashboard.identity)
    guard permissions.canCreateVm else { return "Your account on this server is not active." }
    if dashboard.backend == .managed {
      guard permissions.isAdmin else {
        return "Only administrators can create VMs on this managed server."
      }
      guard dashboard.supports("create_vm"), dashboard.supports("list_image_catalog") else {
        return "This server has not enabled VM creation from approved images."
      }
    }
    return dashboard.canCreateManagedVm ? nil : "VM creation is unavailable on this server."
  }

  /// Why the Enroll VM menu cannot enroll on this server, or nil when it can.
  func enrollVmIssue(forServer id: UUID) -> String? {
    guard let dashboard = dashboard(forServer: id) else {
      return "Load this server's dashboard first."
    }
    guard Permissions(identity: dashboard.identity).isAdmin else {
      return "Administrator access on this server is required."
    }
    guard dashboard.supports("plan_vm_enrollment"), dashboard.supports("enroll_vm") else {
      return "This server has not enabled VM enrollment."
    }
    return failure(forServer: id) == nil ? nil : "Refresh this server successfully first."
  }
}

enum FleetMaintenance: Hashable {
  case grow, shrink, finishShrink, move
}
