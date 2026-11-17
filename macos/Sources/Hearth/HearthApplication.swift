import AppKit
import HearthCore
import SwiftUI

final class ApplicationDelegate: NSObject, NSApplicationDelegate {
  func applicationDidFinishLaunching(_ notification: Notification) {
    NSApp.setActivationPolicy(.regular)
    NSApp.activate(ignoringOtherApps: true)
  }

  func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
    true
  }
}

@main
struct HearthApplication: App {
  @NSApplicationDelegateAdaptor(ApplicationDelegate.self) private var delegate
  @State private var model = AppModel()

  var body: some Scene {
    WindowGroup("Hearth") {
      RootView()
        .environment(model)
        .frame(minWidth: 920, minHeight: 600)
    }
    .commands {
      CommandMenu("Management") {
        Button("Refresh Current Page") { model.refreshForPage() }
          .keyboardShortcut("r", modifiers: [.command])
          .disabled(
            model.fleetPageActive
              ? model.fleetIsRefreshing : (!model.isConfigured || model.isRefreshing))
        // Auto-refresh is chosen per server; each server polls only itself.
        Menu("Auto-refresh Every \(model.settings.clampedPollingInterval)s") {
          ForEach(model.settings.servers) { server in
            Toggle(
              server.name,
              isOn: Binding(
                get: { model.pollingEnabled(forServer: server.id) },
                set: { model.setPolling($0, forServer: server.id) })
            )
            .disabled(!server.isTrusted)
          }
        }
        .disabled(model.settings.servers.isEmpty)
      }
    }

    Settings {
      SettingsView()
        .environment(model)
    }
  }
}
