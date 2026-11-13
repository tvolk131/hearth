import AppKit
import PveDevCore
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
struct PveDevApplication: App {
  @NSApplicationDelegateAdaptor(ApplicationDelegate.self) private var delegate
  @State private var model = AppModel()

  var body: some Scene {
    WindowGroup("PVE Dev Management") {
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
        Toggle(
          "Auto-refresh (fleet pages or selected server) Every \(model.settings.clampedPollingInterval)s",
          isOn: Binding(get: { model.settings.pollingEnabled }, set: { model.setPolling($0) })
        )
        .disabled(!model.isConfigured)
      }
    }

    Settings {
      SettingsView()
        .environment(model)
    }
  }
}
