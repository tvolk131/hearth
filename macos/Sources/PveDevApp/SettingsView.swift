import AppKit
import PveDevCore
import SwiftUI
import UniformTypeIdentifiers

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @State private var draft = AppSettings()
    @State private var baselineRevision: UUID?
    @State private var isInspecting = false
    @State private var inspectionFailure: RpcFailure?

    var body: some View {
        Form {
            Section("Backend") {
                pathRow(title: "pve-devctl executable", path: $draft.executablePath,
                        prompt: "/usr/local/bin/pve-devctl", choose: chooseExecutable)
                pathRow(title: "Inventory executable", path: Binding(
                    get: { draft.inventoryExecutablePath ?? "" }, set: { draft.inventoryExecutablePath = $0 }),
                    prompt: "pve-dev-inventory beside pve-devctl", choose: chooseInventoryExecutable)
                Text("Read-only hosts use pve-dev-inventory. Leave its path empty to use the executable beside pve-devctl.")
                    .font(.caption).foregroundStyle(.secondary)
                Text("Rust opens profiles and private keys. The app stores profile paths and public server, environment, and device identities.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Servers") {
                if !draft.servers.isEmpty {
                    Picker("Server", selection: $draft.selectedServerId) {
                        ForEach(draft.servers) { server in
                            Text(server.name).tag(Optional(server.id))
                        }
                    }
                }
                HStack {
                    Button("Add Controller", systemImage: "plus") {
                        let server = ServerProfile(name: "Server \(draft.servers.count + 1)")
                        draft.servers.append(server)
                        draft.selectedServerId = server.id
                        inspectionFailure = nil
                    }.disabled(draft.servers.count >= 32)
                    Button("Add Read-only Host", systemImage: "eye") {
                        let server = ServerProfile(name: "Host \(draft.servers.count + 1)", mode: .observation)
                        draft.servers.append(server)
                        draft.selectedServerId = server.id
                        inspectionFailure = nil
                    }.disabled(draft.servers.count >= 32)
                    if let id = draft.selectedServerId {
                        Button("Remove Server", systemImage: "minus") {
                            draft.servers.removeAll { $0.id == id }
                            draft.selectedServerId = draft.servers.first?.id
                            inspectionFailure = nil
                        }
                    }
                }
                if let index = draft.servers.firstIndex(where: { $0.id == draft.selectedServerId }) {
                    TextField("Name", text: $draft.servers[index].name)
                    Picker("Connection", selection: Binding(
                        get: { draft.servers[index].mode ?? .controller },
                        set: { draft.servers[index].mode = $0; draft.servers[index].identity = nil; draft.servers[index].observationIdentity = nil })) {
                            Text("Controller (iroh)").tag(ServerProfile.Mode.controller)
                            Text("Read-only host (pinned SSH)").tag(ServerProfile.Mode.observation)
                        }
                    pathRow(title: "Profile JSON", path: Binding(
                        get: { draft.servers[index].profilePath },
                        set: { draft.servers[index].profilePath = $0; draft.servers[index].identity = nil; draft.servers[index].observationIdentity = nil }),
                        prompt: "/Users/you/.config/pve-dev/profile.json", choose: chooseProfile)
                    if let identity = draft.servers[index].observationIdentity, draft.servers[index].isReadOnly {
                        LabeledContent("Node") { identityText(identity.node) }
                        LabeledContent("SSH host key") { identityText(identity.hostKeySha256) }
                        LabeledContent("Machine ID hash") { identityText(identity.machineIdSha256) }
                        LabeledContent("ZFS pool GUID") { identityText(identity.zpoolGuid) }
                    } else if let identity = draft.servers[index].identity {
                        LabeledContent("Server ID") { identityText(identity.serverId) }
                        LabeledContent("Environment") { identityText(identity.environment) }
                        LabeledContent("Device ID") { identityText(identity.deviceId) }
                    } else {
                        Text("This profile needs inspection before the app can connect.").foregroundStyle(.secondary)
                    }
                    Text(draft.servers[index].isReadOnly
                         ? "Trust the reviewed node, SSH key, machine identity and pool pins in this local profile. Refresh runs only the fixed read-only collector. This does not adopt VMs or authorize changes."
                         : "Inspecting trusts the public identities in the selected local profile. Every request verifies them again. Replacing this profile cannot redirect saved retries to another server.")
                        .font(.caption).foregroundStyle(.secondary)
                    HStack {
                        if isInspecting { ProgressView().controlSize(.small) }
                        Button("Save and Trust Profile") { inspect() }
                            .disabled(isInspecting || !draft.validationIssues().isEmpty)
                    }
                }
                if let failure = inspectionFailure { FailureBox(title: failure.title, detail: failure.detail) }
                Text("Removing a server keeps its pending requests for review. Stopped VM transfers require trusted administrator profiles and migration support on both servers.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .disabled(isInspecting)
            Section("Requests") {
                Stepper("Request timeout: \(draft.timeoutSeconds)s", value: $draft.timeoutSeconds,
                        in: AppSettings.timeoutRange, step: 5)
                Stepper("Auto-refresh interval: \(draft.pollingIntervalSeconds)s", value: $draft.pollingIntervalSeconds,
                        in: AppSettings.pollingRange, step: 5)
                Text("Auto-refresh follows the current fleet page or selected server; failed hosts pause until manually refreshed. Enable it from Management ▸ Auto-refresh; it starts off at every launch.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            IssueList(messages: draft.validationIssues())
            HStack {
                Spacer()
                Button("Revert") { reload() }
                    .disabled((draft == model.settings && baselineRevision == model.settingsRevision) || isInspecting)
                Button("Save") { saveDraft() }
                    .keyboardShortcut(.defaultAction)
                    .disabled((draft == model.settings && baselineRevision == model.settingsRevision) || isInspecting || !draft.serverListIssues.isEmpty)
            }
        }
        .formStyle(.grouped)
        .frame(width: 640, height: 760)
        .onAppear { reload() }
    }

    private func reload() {
        draft = model.settings
        baselineRevision = model.settingsRevision
        inspectionFailure = nil
    }

    @discardableResult
    private func saveDraft() -> Bool {
        guard let baselineRevision, model.applySettings(draft, expectedRevision: baselineRevision) else {
            inspectionFailure = .configuration("Settings changed in another window. Use Revert to load the current settings before saving.")
            return false
        }
        draft = model.settings
        self.baselineRevision = model.settingsRevision
        return true
    }

    private func inspect() {
        guard let id = draft.selectedServerId else { return }
        guard saveDraft() else { return }
        inspectionFailure = nil
        isInspecting = true
        Task {
            do throws(RpcFailure) { try await model.pinServer(id) }
            catch { inspectionFailure = error }
            draft = model.settings
            baselineRevision = model.settingsRevision
            isInspecting = false
        }
    }

    private func identityText(_ value: String) -> some View {
        Text(value).font(.caption.monospaced()).lineLimit(1).truncationMode(.middle).textSelection(.enabled).help(value)
    }

    private func pathRow(title: String, path: Binding<String>, prompt: String, choose: @escaping () -> Void) -> some View {
        HStack {
            TextField(title, text: path, prompt: Text(prompt)).autocorrectionDisabled().truncationMode(.middle)
            Button("Choose…", action: choose)
        }
    }

    private func chooseExecutable() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.treatsFilePackagesAsDirectories = true
        panel.message = "Choose the pve-devctl executable"
        if panel.runModal() == .OK, let url = panel.url { draft.executablePath = url.standardizedFileURL.path }
    }

    private func chooseInventoryExecutable() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = "Choose the pve-dev-inventory executable"
        if panel.runModal() == .OK, let url = panel.url { draft.inventoryExecutablePath = url.standardizedFileURL.path }
    }

    private func chooseProfile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json]
        panel.message = "Choose the pve-dev profile JSON"
        if panel.runModal() == .OK, let url = panel.url,
           let index = draft.servers.firstIndex(where: { $0.id == draft.selectedServerId }) {
            draft.servers[index].profilePath = url.standardizedFileURL.path
            draft.servers[index].identity = nil
            draft.servers[index].observationIdentity = nil
        }
    }
}
