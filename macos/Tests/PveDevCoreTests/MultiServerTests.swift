import Foundation
import Testing

@testable import PveDevCore

@Suite("Server settings and request binding")
struct MultiServerTests {
  let identity = ServerIdentity(
    serverId: String(repeating: "a", count: 64), environment: String(repeating: "b", count: 32),
    deviceId: String(repeating: "c", count: 64))

  @Test func legacySettingsMigrateOnceWithoutInventingTrust() throws {
    let suite = "PveDevApp.migration.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let legacy = Data(
      #"{"executablePath":"/usr/bin/true","profilePath":"/tmp/old.json","timeoutSeconds":45,"pollingEnabled":true,"pollingIntervalSeconds":30}"#
        .utf8)
    defaults.set(legacy, forKey: SettingsStore.legacyDefaultsKey)
    let store = SettingsStore(suiteName: suite)
    let migrated = store.load()
    #expect(migrated.servers.count == 1)
    #expect(migrated.selectedServer?.profilePath == "/tmp/old.json")
    #expect(migrated.selectedServer?.identity == nil)
    #expect(!migrated.pollingEnabled)
    #expect(migrated.timeoutSeconds == 45)
    #expect(store.load().selectedServerId == migrated.selectedServerId)
    #expect(defaults.data(forKey: SettingsStore.legacyDefaultsKey) == legacy)
    defaults.set(Data("broken".utf8), forKey: SettingsStore.defaultsKey)
    #expect(store.load().servers.isEmpty)  // Do not resurrect an obsolete legacy route.
  }

  @Test func settingsRoundTripKeepsNamedIdentitiesButDisablesPolling() throws {
    var settings = AppSettings()
    let a = ServerProfile(name: "HDD", profilePath: "/tmp/a.json", identity: identity)
    let b = ServerProfile(name: "SSD", profilePath: "/tmp/b.json")
    settings.servers = [a, b]
    settings.selectedServerId = b.id
    settings.pollingEnabled = true
    let restored = try JSONDecoder().decode(AppSettings.self, from: JSONEncoder().encode(settings))
    #expect(restored.servers == [a, b])
    #expect(restored.selectedServerId == b.id)
    #expect(!restored.pollingEnabled)
  }

  @Test func expectedIdentitiesArePassedAsSeparateArguments() {
    let config = RpcConfiguration(
      executableURL: URL(fileURLWithPath: "/tmp/cli"), profilePath: "/tmp/a profile.json",
      expectedIdentity: identity, limits: ProcessLimits())
    #expect(
      config.arguments == [
        "--profile", "/tmp/a profile.json", "--expected-server-id", identity.serverId,
        "--expected-environment", identity.environment, "--expected-device-id", identity.deviceId,
        "rpc",
      ])
    #expect(identity.isValid)
    #expect(
      !ServerIdentity(
        serverId: "short", environment: identity.environment, deviceId: identity.deviceId
      ).isValid)
  }

  @Test func sameVmIdAndWireIdRemainIndependentOnDifferentServers() throws {
    let a = UUID()
    let b = UUID()
    let request = RpcRequest.vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete))
    var ledger = MutationLedger()
    let first = ledger.prepare(request, profilePath: "/tmp/a.json", serverId: a, identity: identity)
    #expect(ledger.isBusy(vmId: 9001, serverId: a, identity: identity))
    #expect(!ledger.isBusy(vmId: 9001, serverId: b, identity: identity))
    let second = ledger.prepare(
      request, profilePath: "/tmp/b.json", serverId: b, identity: identity)
    #expect(first.id != second.id)
    #expect(first.wireRequestId == second.wireRequestId)
    #expect(throws: LedgerError.serverMismatch) {
      try ledger.beginAttempt(first.id, profilePath: "/tmp/a.json", serverId: b, identity: identity)
    }
    let replacement = ServerIdentity(
      serverId: String(repeating: "d", count: 64), environment: identity.environment,
      deviceId: identity.deviceId)
    #expect(throws: LedgerError.serverMismatch) {
      try ledger.beginAttempt(
        first.id, profilePath: "/tmp/a.json", serverId: a, identity: replacement)
    }
    #expect(ledger.entry(first.id)?.attempts == 0)
    _ = try ledger.beginAttempt(
      first.id, profilePath: "/tmp/a.json", serverId: a, identity: identity)
    ledger.recordAcknowledged(first.id)
    #expect(ledger.entries.map(\.id) == [second.id])
  }

  @Test func legacyLedgerIsReadableButCannotAcquireIdentityOnRetry() throws {
    var ledger = MutationLedger()
    let old = ledger.prepare(
      .vmAction(VmActionRequest(requestId: UUID(), vmId: 9001, kind: .delete)),
      profilePath: "/tmp/a.json")
    // Old files omit the new optional fields entirely.
    var object = try #require(
      JSONSerialization.jsonObject(with: JSONEncoder().encode(ledger)) as? [String: Any])
    var entries = try #require(object["entries"] as? [[String: Any]])
    entries[0].removeValue(forKey: "serverId")
    entries[0].removeValue(forKey: "identity")
    object["entries"] = entries
    ledger = try JSONDecoder().decode(
      MutationLedger.self, from: JSONSerialization.data(withJSONObject: object))
    #expect(ledger.entry(old.id)?.canRetry == false)
    #expect(throws: LedgerError.unboundIdentity) {
      try ledger.beginAttempt(
        old.id, profilePath: "/tmp/a.json", serverId: UUID(), identity: identity)
    }
  }
  @Test func serverNamesCannotMakeQualifiedTargetsAmbiguous() {
    var settings = AppSettings()
    settings.servers = [
      ServerProfile(name: "PVE", profilePath: "/a"),
      ServerProfile(name: " pve ", profilePath: "/b"),
    ]
    settings.selectedServerId = settings.servers.first?.id
    #expect(settings.serverListIssues.contains { $0.contains("names must be unique") })
  }

}
