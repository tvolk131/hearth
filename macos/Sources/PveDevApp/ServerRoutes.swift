import Foundation
import PveDevCore

/// Route checks and read-only queries bound to a captured `MutationOrigin`.
/// Forms, plans, inspections and the two-server migration workflow use these so
/// a late reply can never authorize work on another server or a replaced profile.
@MainActor
final class ServerRoutes {
  private let registry: ServerRegistry
  private let client: RpcClient

  init(registry: ServerRegistry, client: RpcClient) {
    self.registry = registry
    self.client = client
  }

  /// Plans and inspections belong to the exact route captured when their form
  /// opened. A late reply must never authorize a mutation on another server.
  func query<Value: Decodable & Sendable>(
    _ request: RpcRequest, origin: MutationOrigin?, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    guard let origin, origin == registry.requestOrigin else {
      throw .configuration(
        "The server or its profile changed. Open this action again on its original server.")
    }
    let value = try await queryOnRoute(request, origin: origin, as: type)
    guard origin == registry.requestOrigin else {
      throw .configuration(
        "The server or its profile changed while this request was running. Its result was discarded."
      )
    }
    return value
  }

  /// Used by a reviewed two-server workflow. Both routes carry independently
  /// pinned identities; choosing a destination never changes the selected host.
  func queryOnRoute<Value: Decodable & Sendable>(
    _ request: RpcRequest, origin: MutationOrigin, as type: Value.Type
  ) async throws(RpcFailure) -> Value {
    guard !request.isMutation else {
      throw .configuration(
        "This request changes server state and must use the pending-request ledger.")
    }
    let configuration = try checkedRoute(request, origin: origin)
    let value = try await client.query(request, configuration: configuration, as: type)
    guard origin == registry.origin(forServer: origin.serverId) else {
      throw .configuration(
        "The server profile changed while this request was running. Its result was discarded.")
    }
    return value
  }

  func checkedRoute(_ request: RpcRequest, origin: MutationOrigin) throws(RpcFailure)
    -> RpcConfiguration
  {
    guard origin == registry.origin(forServer: origin.serverId),
      let server = registry.server(origin.serverId), !server.isReadOnly
    else {
      throw .configuration("This server route is read-only or changed after review.")
    }
    let dashboard = registry.sessions[server.id]?.dashboard
    if request.requiresAdministrator && !Permissions(identity: dashboard?.identity).isAdmin {
      throw .configuration("Load an administrator dashboard for this server before continuing.")
    }
    if let issue = routeIssue(request, serverId: server.id) { throw .configuration(issue) }
    if case .vmAction(let action) = request, action.kind == .start,
      let vm = dashboard?.vms.first(where: { $0.vmId == action.vmId }),
      let issue = startIssue(vm: vm, on: server.id)
    {
      throw .configuration(issue)
    }
    return try registry.configuration(for: server.id)
  }

  func routeIssue(_ request: RpcRequest, serverId: UUID?) -> String? {
    guard let serverId, let session = registry.sessions[serverId] else {
      return "The server route is unavailable."
    }
    let expectedBackend = registry.server(serverId)?.expectedBackend
    if let dashboard = session.dashboard {
      if let expectedBackend, dashboard.backend != expectedBackend {
        return "The controller backend differs from this profile's expected backend."
      }
      return dashboard.requestIssue(request, catalog: session.imageCatalog)
    }
    if request != .dashboard && expectedBackend != nil {
      return "Load this controller's dashboard before continuing."
    }
    // Preserve legacy lab/test flows; capability-dependent requests still require a dashboard.
    if request == .listImageCatalog || request.requiredCapability != nil {
      return "Load this server's dashboard before continuing."
    }
    return nil
  }

  /// A fleet Start requires affirmative, recent observations from every peer.
  /// Independent controllers cannot provide a distributed exclusion lock.
  func startIssue(vm: Vm, on serverId: UUID, now: Date = Date()) -> String? {
    for peer in registry.settings.servers where !peer.isReadOnly && peer.id != serverId {
      guard let session = registry.sessions[peer.id], let dashboard = session.dashboard,
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
}
