import Foundation
import Observation
import HearthCore

/// Mutations: every change request, from a form, a retry or an awaited reviewed
/// workflow, takes one path through the durable `MutationLedger`. A request ID
/// is minted once, recorded before launch, and kept until the server acknowledges it.
@MainActor
@Observable
final class MutationCoordinator {
  private(set) var ledger: MutationLedger
  private(set) var ledgerStorageIssue: String?
  private(set) var mutationSendStates: [UUID: RpcSendState] = [:]
  private var launchRefusals: [UUID: String] = [:]

  /// The one in-flight task per pending request, whichever way it was delivered.
  @ObservationIgnored private var mutationTasks:
    [UUID: Task<Result<MutationAcknowledgement, RpcFailure>, Never>] = [:]
  @ObservationIgnored private let registry: ServerRegistry
  @ObservationIgnored private let routes: ServerRoutes
  @ObservationIgnored private let refresher: RefreshCoordinator
  @ObservationIgnored private let notices: NoticeCenter
  @ObservationIgnored private let client: RpcClient
  @ObservationIgnored private let ledgerStore: LedgerStore
  @ObservationIgnored private let ledgerLoadFailed: Bool

  init(
    registry: ServerRegistry, routes: ServerRoutes, refresher: RefreshCoordinator,
    notices: NoticeCenter, client: RpcClient, ledgerStore: LedgerStore
  ) {
    self.registry = registry
    self.routes = routes
    self.refresher = refresher
    self.notices = notices
    self.client = client
    self.ledgerStore = ledgerStore
    do {
      ledger = try ledgerStore.load()
      ledgerLoadFailed = false
    } catch {
      ledger = MutationLedger()
      ledgerLoadFailed = true
      ledgerStorageIssue =
        "Saved pending requests could not be read. They were left untouched on disk at \(ledgerStore.fileURL.path)."
    }
  }

  // MARK: Delivery

  /// How a caller learns the outcome. Background sends (forms and manual
  /// retries) report through notices and refresh on completion; awaited sends
  /// (reviewed sheets and the migration workflow) receive the typed result.
  private enum Delivery { case background, awaited }

  /// Why the shared path stopped before launching, reported per delivery.
  private enum Refusal: Error {
    /// Background: nothing to report (the reason is already visible or recorded).
    case silent
    /// Background: post "Not sent" for this route.
    case notSent(String, MutationOrigin?)
    /// Awaited: throw this failure.
    case failure(RpcFailure)

    var awaitedFailure: RpcFailure {
      switch self {
      case .failure(let failure): failure
      case .notSent(let detail, _): .configuration(detail)
      case .silent: .configuration("The pending request changed.")
      }
    }
  }

  private struct Launch {
    let task: Task<Result<MutationAcknowledgement, RpcFailure>, Never>
    let request: RpcRequest
    let server: ServerProfile
  }

  @discardableResult
  func submit(_ request: RpcRequest, origin: MutationOrigin?) -> UUID? {
    let id: UUID
    do throws(Refusal) {
      id = try admit(request, origin: origin, delivery: .background)
    } catch {
      report(error)
      return nil
    }
    send(id, origin: origin)
    return id
  }

  func retry(_ id: UUID) { send(id, origin: nil) }

  /// Returns typed migration grants/receipts only after the same durable
  /// request bookkeeping used by ordinary forms. Unknown replies remain
  /// retryable in Pending Requests with their original route and payload.
  func performMutation(_ request: RpcRequest, origin: MutationOrigin) async throws(RpcFailure)
    -> MutationAcknowledgement
  {
    let id: UUID
    let launch: Launch
    do throws(Refusal) {
      id = try admit(request, origin: origin, delivery: .awaited)
      launch = try self.launch(id, origin: origin, delivery: .awaited)
    } catch { throw error.awaitedFailure }
    let task = launch.task
    let outcome = await withTaskCancellationHandler {
      await task.value
    } onCancel: {
      task.cancel()
    }
    complete(
      id, request: launch.request, server: launch.server, revision: origin.revision,
      outcome: outcome)
    switch outcome {
    case .success(let acknowledgement):
      guard origin == registry.origin(forServer: launch.server.id) else {
        throw .configuration(
          "The original server acknowledged this action, but its profile changed. Refresh that server before continuing."
        )
      }
      return acknowledgement
    case .failure(let failure): throw failure
    }
  }

  private func send(_ id: UUID, origin: MutationOrigin?) {
    do throws(Refusal) {
      _ = try launch(id, origin: origin, delivery: .background)
    } catch { report(error) }
  }

  private func report(_ refusal: Refusal) {
    switch refusal {
    case .silent: break
    case .notSent(let detail, let origin): postNotSent(detail, origin: origin)
    case .failure(let failure): postNotSent(failure.detail, origin: nil)
    }
  }

  /// Records a new request in the ledger, or finds the identical pending one.
  private func admit(_ request: RpcRequest, origin: MutationOrigin?, delivery: Delivery)
    throws(Refusal) -> UUID
  {
    let background = delivery == .background
    guard request.isMutation else {
      throw background ? .silent : .failure(.configuration("This action is read-only."))
    }
    guard let origin else {
      throw .notSent("The original server route is unavailable.", origin)
    }
    if !background && ledgerLoadFailed {
      throw .failure(
        .configuration(
          "The saved pending-request ledger needs investigation before sending changes."))
    }
    let configuration: RpcConfiguration
    do { configuration = try routes.checkedRoute(request, origin: origin) } catch {
      throw background ? .notSent(error.detail, origin) : .failure(error)
    }
    if background && ledgerLoadFailed {
      throw .notSent("The pending-request ledger needs investigation.", origin)
    }
    guard let server = registry.server(origin.serverId), let identity = server.identity else {
      throw background ? .silent : .failure(.configuration("The server identity is unavailable."))
    }
    let entry = ledger.prepare(
      request, profilePath: configuration.profilePath, serverId: server.id, identity: identity)
    if background { guard persistLedger() else { throw .silent } }
    guard entry.request == request else {
      throw background
        ? .notSent(
          "A matching request already exists. Review it in Pending Requests and explicitly retry with the same request ID.",
          origin)
        : .failure(
          .configuration(
            "A matching request is already pending. Use Retry with Same Request ID in Pending Requests on \(server.name)."
          ))
    }
    return entry.id
  }

  /// The single send path: rechecks the route, marks the attempt in flight,
  /// persists the ledger before anything launches, then starts one child process.
  private func launch(_ id: UUID, origin: MutationOrigin?, delivery: Delivery)
    throws(Refusal) -> Launch
  {
    let background = delivery == .background
    guard !ledgerLoadFailed, let entry = ledger.entry(id) else { throw .silent }
    // A manual retry goes only to the entry's own server, and only while that
    // server keeps the profile path and identity the request was recorded with.
    // New forms pass their reviewed origin. Neither depends on the selected server.
    guard let origin = origin ?? routableOrigin(entry),
      let server = registry.server(origin.serverId),
      let identity = server.identity,
      registry.sessions[server.id] != nil
    else { throw .silent }
    let configuration: RpcConfiguration
    do { configuration = try routes.checkedRoute(entry.request, origin: origin) } catch {
      throw background ? .notSent(error.detail, origin) : .failure(error)
    }
    let request: RpcRequest
    do {
      request = try ledger.beginAttempt(
        id, profilePath: configuration.profilePath, serverId: server.id, identity: identity)
    } catch {
      throw background
        ? .notSent(Self.describe(error), origin)
        : .failure(.configuration(Self.describe(error)))
    }
    guard persistLedger() else {
      let reason =
        background
        ? "The pending request could not be durably saved. Nothing was sent."
        : "The pending request could not be saved. Nothing was sent."
      ledger.recordNotLaunched(id, reason: reason)
      throw background ? .silent : .failure(.configuration(reason))
    }
    let client = self.client
    let reviewedVm = reviewedMachine(request, on: origin.serverId)
    let revision = origin.revision
    mutationSendStates[id] = .queued
    let task = Task { [weak self] () -> Result<MutationAcknowledgement, RpcFailure> in
      let outcome: Result<MutationAcknowledgement, RpcFailure>
      do throws(RpcFailure) {
        outcome = .success(
          try await client.submit(
            request, configuration: configuration,
            onState: { [weak self] state in await self?.setSendState(state, id: id) },
            shouldLaunch: { [weak self] in
              await self?.authorizeLaunch(
                id, request: request, origin: origin, configuration: configuration,
                reviewedVm: reviewedVm) ?? false
            }))
      } catch { outcome = .failure(error) }
      if background {
        self?.complete(id, request: request, server: server, revision: revision, outcome: outcome)
      }
      return outcome
    }
    mutationTasks[id] = task
    return Launch(task: task, request: request, server: server)
  }

  private func reviewedMachine(_ request: RpcRequest, on id: UUID) -> Vm? {
    if case .vmAction(let action) = request {
      return registry.sessions[id]?.dashboard?.vms.first { $0.vmId == action.vmId }
    }
    return nil
  }

  /// MainActor gate + sending transition are atomic relative to Settings and
  /// dashboard updates. Later cancellation is conservatively "may have sent".
  private func authorizeLaunch(
    _ id: UUID, request: RpcRequest, origin: MutationOrigin,
    configuration: RpcConfiguration, reviewedVm: Vm?
  ) -> Bool {
    do throws(RpcFailure) {
      guard try routes.checkedRoute(request, origin: origin) == configuration else {
        throw .configuration("The server configuration changed while this request was queued.")
      }
      guard let entry = ledger.entry(id), entry.isInFlight else {
        throw .configuration("The pending request changed.")
      }
      let dashboard = registry.sessions[origin.serverId]?.dashboard
      if entry.attempts == 1, let reviewedVm, case .vmAction(let action) = request {
        guard let current = reviewedMachine(request, on: origin.serverId), current == reviewedVm,
          dashboard?.availableActions(for: current).contains(action.kind) == true,
          dashboard?.operations.contains(where: {
            $0.vmId == action.vmId && !$0.status.isTerminal
          }) != true
        else {
          throw .configuration(
            "The VM changed while this request was queued. Refresh and review it again.")
        }
      }
      mutationSendStates[id] = .sending
      return true
    } catch {
      launchRefusals[id] = error.detail
      return false
    }
  }

  private func setSendState(_ state: RpcSendState, id: UUID) { mutationSendStates[id] = state }

  private func complete(
    _ id: UUID, request: RpcRequest, server: ServerProfile, revision: UUID,
    outcome: Result<MutationAcknowledgement, RpcFailure>
  ) {
    mutationTasks[id] = nil
    let sendState = mutationSendStates.removeValue(forKey: id)
    let refusal = launchRefusals.removeValue(forKey: id)
    let changedRoute = registry.sessions[server.id]?.revision != revision
    switch outcome {
    case .success(let acknowledgement):
      ledger.recordAcknowledged(id)
      notices.post(
        .success, "\(server.name): \(request.summary)", acknowledgement.summary,
        serverId: server.id, identity: server.identity, global: changedRoute)
    case .failure(let failure):
      let cancelledInQueue = failure == .cancelled && sendState == .queued
      if cancelledInQueue {
        ledger.recordNotLaunched(id)
      } else {
        // The ledger also records queue-wait timeouts as not sent.
        ledger.recordFailure(id, failure: failure)
      }
      if cancelledInQueue || failure.provesNotSent {
        notices.post(
          .failure, "\(server.name): Not sent",
          refusal
            ?? (cancelledInQueue
              ? "Cancelled before this attempt launched." : failure.detail)
            + " The original request ID is retained.",
          global: true)
      } else {
        // A retried conflict stays unacknowledged in the ledger; only a
        // recorded rejection is final.
        let rejected = ledger.entry(id)?.phase == .rejected
        notices.post(
          .failure, "\(server.name): \(request.summary): \(failure.title)",
          failure.detail
            + (rejected
              ? ""
              : "\nThe request ID was kept. Retry from Pending Requests to avoid a duplicate."),
          serverId: server.id, identity: server.identity, global: changedRoute)
      }
    }
    persistLedger()
    // Finishing an A action must never refresh selected server B or a replacement A profile.
    if registry.sessions[server.id]?.revision == revision && sendState != .queued {
      refresher.refresh(server.id)
    }
  }

  @discardableResult
  private func persistLedger() -> Bool {
    guard !ledgerLoadFailed else { return false }
    do {
      try ledgerStore.save(ledger)
      ledgerStorageIssue = nil
      return true
    } catch {
      ledgerStorageIssue =
        "Pending requests could not be saved. New sends are blocked until the ledger can be written. Already accepted server work is not cancelled."
      return false
    }
  }

  private func postNotSent(_ detail: String, origin: MutationOrigin?) {
    let server = registry.server(origin?.serverId)
    let unchanged =
      origin != nil && origin == origin.flatMap { registry.origin(forServer: $0.serverId) }
    notices.post(
      .failure, "\(server?.name ?? "Original server"): Not sent", detail,
      serverId: server?.id, identity: server?.identity, global: !unchanged)
  }

  private static func describe(_ error: LedgerError) -> String {
    switch error {
    case .unknownMutation: "The request is no longer pending."
    case .alreadyInFlight: "This request is already in progress."
    case .rejectedByServer:
      "The server rejected this request. Discard it and submit a corrected one."
    case .profileMismatch:
      "This request belongs to a different profile path. Restore the original profile to retry."
    case .serverMismatch:
      "This request belongs to a different server or device identity. Switch to its original server to retry."
    case .unboundIdentity:
      "This older request has no saved server identity and cannot be safely retried. Inspect it before discarding."
    }
  }

  // MARK: Pending requests

  var pendingForCurrentProfile: [PendingMutation] { ledger.entries.filter(isCurrent) }
  var pendingForOtherProfiles: [PendingMutation] { ledger.entries.filter { !isCurrent($0) } }
  var unresolvedCount: Int { ledger.entries.filter { $0.phase != .rejected }.count }

  func canRetry(_ entry: PendingMutation) -> Bool { retryIssue(entry) == nil }
  /// Why Retry with Same Request ID is unavailable for this entry, or nil when it is available.
  func retryIssue(_ entry: PendingMutation) -> String? {
    guard entry.canRetry else {
      if entry.phase == .rejected {
        return "The server rejected this request. Discard it and submit a corrected one."
      }
      if entry.isInFlight { return "This request is already in progress." }
      return "This request has no saved server identity, so it cannot be retried safely."
    }
    guard let server = registry.server(entry.serverId) else {
      return "Its server was removed from Settings. Verify the outcome there before discarding it."
    }
    guard isRoutable(entry) else {
      return server.isReadOnly
        ? "\(server.name) is now a read-only host and cannot receive this request."
        : "\(server.name) now uses a different profile or identity. Restore its original profile to retry."
    }
    guard server.isTrusted else {
      return "Inspect and trust \(server.name)'s profile in Settings before retrying."
    }
    do { _ = try registry.configuration(for: server.id) } catch { return error.detail }
    if let issue = routes.routeIssue(entry.request, serverId: server.id) { return issue }
    if entry.request.requiresAdministrator
      && !Permissions(identity: registry.sessions[server.id]?.dashboard?.identity).canManageUsers
    {
      return "Administrator access on \(server.name) is required. Refresh its dashboard."
    }
    return nil
  }
  func cancelMutation(_ id: UUID) { mutationTasks[id]?.cancel() }
  func discard(_ id: UUID) { if ledger.discard(id) { persistLedger() } }
  func isMutationInFlight(_ id: UUID) -> Bool { mutationTasks[id] != nil }
  func isVmBusy(_ vmId: UInt16, on serverId: UUID) -> Bool {
    guard let server = registry.server(serverId), let identity = server.identity else {
      return false
    }
    return registry.sessions[serverId]?.dashboard?.operations.contains(where: {
      $0.vmId == vmId && $0.status.isActive
    }) == true
      || ledger.isBusy(vmId: vmId, serverId: serverId, identity: identity)
  }
  func isVmBusy(_ vmId: UInt16) -> Bool {
    guard let server = registry.selectedServer, let identity = server.identity else {
      return false
    }
    if registry.selectedSession.dashboard?.operations.contains(where: {
      $0.vmId == vmId && $0.status.isActive
    }) == true {
      return true
    }
    return ledger.isBusy(vmId: vmId, serverId: server.id, identity: identity)
  }
  func inFlightEntry(vmId: UInt16, on serverId: UUID) -> PendingMutation? {
    ledger.entries.first { entry in
      guard entry.serverId == serverId, entry.phase == .inFlight else { return false }
      if case .vmAction(let action) = entry.request { return action.vmId == vmId }
      return false
    }
  }
  func sendState(vmId: UInt16, on serverId: UUID) -> RpcSendState? {
    inFlightEntry(vmId: vmId, on: serverId).flatMap { mutationSendStates[$0.id] }
  }
  func serverName(for entry: PendingMutation) -> String {
    registry.server(entry.serverId)?.name
      ?? (entry.hasServerBinding ? "Removed server" : "Legacy request (identity unknown)")
  }

  /// Whether the entry's own server still has the profile path and identity the
  /// request was recorded with, so a retry reaches exactly that server.
  func isRoutable(_ entry: PendingMutation) -> Bool {
    guard entry.hasServerBinding, let server = registry.server(entry.serverId), !server.isReadOnly,
      server.identity != nil
    else { return false }
    return entry.identity == server.identity && entry.profilePath == server.canonicalProfilePath
  }

  private func routableOrigin(_ entry: PendingMutation) -> MutationOrigin? {
    guard isRoutable(entry), let serverId = entry.serverId else { return nil }
    return registry.origin(forServer: serverId)
  }

  private func isCurrent(_ entry: PendingMutation) -> Bool {
    !registry.isReadOnly && entry.hasServerBinding && entry.serverId == registry.selectedServerId
      && entry.identity == registry.selectedServer?.identity
      && entry.profilePath == registry.currentProfilePath
  }
}
