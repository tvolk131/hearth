import Foundation
import Testing
@testable import PveDevCore

@Suite("Isolated persistent app bootstrap")
struct BootstrapSettingsTests {
    func seed() -> AppSettings {
        var s = AppSettings(executablePath: "/bundle/Contents/Resources/bin/pve-devctl")
        s.servers = ["a", "d"].map { n in
            ServerProfile(name: "Persistent \(n)", profilePath: "/persistent/\(n).json",
                identity: ServerIdentity(serverId: String(repeating: n, count: 64), environment: String(repeating: n, count: 32), deviceId: String(repeating: n, count: 64)), expectedBackend: .managed)
        }
        s.selectedServerId = s.servers[0].id
        return s
    }
    @Test func twoPinnedControllersSeedNewSuiteAndExistingSettingsWin() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: dir) }
        let suite = "PveDevApp.bootstrap-tests.\(UUID().uuidString)"
        defer { UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite) }
        let url = dir.appendingPathComponent("seed.json")
        let original = seed(); try JSONEncoder().encode(original).write(to: url)
        let store = SettingsStore(suiteName: suite, bootstrapURL: url)
        let loaded = store.load()
        #expect(loaded.servers == original.servers)
        #expect(loaded.servers.count == 2 && loaded.servers.allSatisfy { $0.expectedBackend == .managed })
        var saved = loaded; saved.servers[0].name = "User's saved label"; store.save(saved)
        try JSONEncoder().encode(seed()).write(to: url)
        #expect(store.load().servers == saved.servers)
        // A corrupt saved suite is not silently replaced by a new seed.
        UserDefaults(suiteName: suite)?.set(Data("corrupt".utf8), forKey: SettingsStore.defaultsKey)
        #expect(store.load().servers.isEmpty)
        #expect(UserDefaults(suiteName: suite)?.data(forKey: SettingsStore.defaultsKey) == Data("corrupt".utf8))
    }
    @Test func unpinnedOrReadOnlySeedDoesNotBootstrapManagement() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: dir) }
        let suite = "PveDevApp.bootstrap-invalid.\(UUID().uuidString)"
        defer { UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite) }
        let url = dir.appendingPathComponent("seed.json")
        var invalid = seed(); invalid.servers[0].identity = nil
        try JSONEncoder().encode(invalid).write(to: url)
        let store = SettingsStore(suiteName: suite, bootstrapURL: url)
        #expect(store.load().servers.isEmpty)
        #expect(UserDefaults(suiteName: suite)?.data(forKey: SettingsStore.defaultsKey) == nil)
        invalid = seed(); invalid.servers[0].mode = .observation
        try JSONEncoder().encode(invalid).write(to: url)
        #expect(store.load().servers.isEmpty)
    }
}
