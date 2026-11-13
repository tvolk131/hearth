import Foundation
import Testing

@testable import PveDevCore

@Suite("Request ID retention")
struct MutationLedgerTests {
  let profile = "/Users/me/profile.json"
  let serverId = UUID()
  let identity = ServerIdentity(
    serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
    deviceId: String(repeating: "c", count: 64))

  func createVm(_ id: UUID = UUID(), name: String = "dev-frank") -> RpcRequest {
    .createVm(
      CreateVmRequest(
        requestId: id, name: name, diskMib: 32, memoryMib: 128, minMemoryMib: 128, cores: 1,
        owner: nil))
  }

  @Test func retryAfterDisconnectionReusesExactRequest() throws {
    var ledger = MutationLedger()
    let original = createVm()
    let entry = ledger.prepare(
      original, profilePath: profile, serverId: serverId, identity: identity)
    #expect(entry.wireRequestId == original.requestId.map(WireUUID.string))

    let firstAttempt = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordFailure(entry.id, failure: .timedOut(seconds: 60))
    #expect(ledger.entry(entry.id)?.phase == .indeterminate)

    let secondAttempt = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    #expect(secondAttempt == firstAttempt)
    #expect(secondAttempt.requestId == original.requestId)
    #expect(try WireCoding.encodeRequest(secondAttempt) == WireCoding.encodeRequest(original))
    #expect(ledger.entry(entry.id)?.attempts == 2)
  }

  @Test(arguments: [
    RpcFailure.cancelled, .launchFailed("x"), .outputTooLarge, .malformedResponse("x"),
    .processFailed(status: 1, signaled: false, stderr: ""),
    .server(ServerError(code: .unavailable, message: "")),
    .server(ServerError(code: .internal, message: "")),
  ])
  func indeterminateFailuresKeepRequestPending(failure: RpcFailure) throws {
    var ledger = MutationLedger()
    let entry = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordFailure(entry.id, failure: failure)
    #expect(ledger.entry(entry.id)?.canRetry == true)
  }

  @Test func acknowledgementClearsEntry() throws {
    var ledger = MutationLedger()
    let entry = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordAcknowledged(entry.id)
    #expect(ledger.entries.isEmpty)
  }

  @Test func duplicateSubmissionReusesPendingRequestId() throws {
    var ledger = MutationLedger()
    let first = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      first.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordFailure(first.id, failure: .timedOut(seconds: 5))
    let second = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    #expect(second.id == first.id)
    #expect(ledger.entries.count == 1)
  }

  @Test func differentIntentGetsSeparateEntry() {
    var ledger = MutationLedger()
    ledger.prepare(
      createVm(name: "one"), profilePath: profile, serverId: serverId, identity: identity)
    ledger.prepare(
      createVm(name: "two"), profilePath: profile, serverId: serverId, identity: identity)
    #expect(ledger.entries.count == 2)
  }

  @Test func inFlightRequestCannotBeSentTwice() throws {
    var ledger = MutationLedger()
    let entry = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    #expect(throws: LedgerError.alreadyInFlight) {
      try ledger.beginAttempt(
        entry.id, profilePath: profile, serverId: serverId, identity: identity)
    }
    #expect(ledger.discard(entry.id) == false)
  }

  @Test func definitiveRejectionIsNotRetriedButFreshSubmissionIsAllowed() throws {
    var ledger = MutationLedger()
    let entry = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordFailure(
      entry.id, failure: .server(ServerError(code: .invalid, message: "over budget")))
    #expect(ledger.entry(entry.id)?.phase == .rejected)
    #expect(throws: LedgerError.rejectedByServer) {
      try ledger.beginAttempt(
        entry.id, profilePath: profile, serverId: serverId, identity: identity)
    }
    let fresh = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    #expect(fresh.id != entry.id)
    #expect(ledger.entries.count == 1)
  }

  @Test func retryIsBoundToOriginalProfile() {
    var ledger = MutationLedger()
    let entry = ledger.prepare(
      createVm(), profilePath: profile, serverId: serverId, identity: identity)
    #expect(throws: LedgerError.profileMismatch) {
      try ledger.beginAttempt(
        entry.id, profilePath: "/other/profile.json", serverId: serverId, identity: identity)
    }
    #expect(ledger.entry(entry.id)?.attempts == 0)
  }

  @Test func vmBusyTracksUnresolvedActions() throws {
    var ledger = MutationLedger()
    let entry = ledger.prepare(
      .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete)),
      profilePath: profile, serverId: serverId, identity: identity)
    #expect(ledger.isBusy(vmId: 9001, serverId: serverId, identity: identity))
    #expect(!ledger.isBusy(vmId: 9002, serverId: serverId, identity: identity))
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordFailure(entry.id, failure: .server(ServerError(code: .forbidden, message: "")))
    #expect(!ledger.isBusy(vmId: 9001, serverId: serverId, identity: identity))
  }

  @Test func persistedLedgerSurvivesRelaunchWithoutAutoSubmit() throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: directory) }
    let store = LedgerStore(fileURL: directory.appendingPathComponent("pending.json"))
    #expect(try store.load().entries.isEmpty)

    var ledger = MutationLedger()
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
    let entry = ledger.prepare(
      request, profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    try store.save(ledger)

    let attributes = try FileManager.default.attributesOfItem(atPath: store.fileURL.path)
    #expect((attributes[.posixPermissions] as? NSNumber)?.intValue == 0o600)

    let reloaded = try store.load()
    let restored = try #require(reloaded.entry(entry.id))
    #expect(restored.request == request)
    #expect(restored.phase == .indeterminate)
    #expect(restored.attempts == 1)
    #expect(restored.lastErrorTitle == "Outcome unknown")
  }
  @Test func cancelledQueuedRetryPreservesEarlierUncertaintyAndRequest() throws {
    var ledger = MutationLedger()
    let request = createVm()
    let entry = ledger.prepare(
      request, profilePath: profile, serverId: serverId, identity: identity)
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordFailure(entry.id, failure: .timedOut(seconds: 60))
    let earlier = try #require(ledger.entry(entry.id))
    _ = try ledger.beginAttempt(
      entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordNotLaunched(entry.id)
    let after = try #require(ledger.entry(entry.id))
    #expect(after.attempts == 1 && after.phase == .indeterminate)
    #expect(after.request == request && after.lastErrorTitle == earlier.lastErrorTitle)
    #expect(after.lastErrorDetail?.contains(earlier.lastErrorDetail!) == true)
    _ = try ledger.beginAttempt(entry.id, profilePath: profile, serverId: serverId, identity: identity)
    ledger.recordNotLaunched(entry.id)
    #expect(ledger.entry(entry.id)?.lastErrorDetail == after.lastErrorDetail)
  }

}
