import HearthCore
import SwiftUI

enum SidebarItem: Hashable {
  case overview
  case machines
  case operations
  case transfers
  case pending
  case users
  case devices
  /// One read-only SSH host's observation page.
  case host(UUID)

  /// Pages that list every controller and filter by server in the page itself.
  var spansAllServers: Bool {
    switch self {
    case .overview, .machines, .operations, .pending: true
    default: false
    }
  }
}

extension AppModel {
  /// Controllers a single-server page can show, for its in-page server picker.
  /// Empty for pages that span every server or belong to one read-only host.
  func scopeServers(for page: SidebarItem) -> [ServerProfile] {
    let admin = fleetServers.filter { isAdmin(onServer: $0.id) }
    switch page {
    case .users: return admin
    case .devices: return admin.filter { dashboard(forServer: $0.id)?.backend != .managed }
    case .transfers:
      return admin.filter { dashboard(forServer: $0.id)?.supports("migration") == true }
    default: return []
    }
  }
}

struct RootView: View {
  @Environment(AppModel.self) private var model
  @State private var selection: SidebarItem? = .overview
  @State private var showCompleted = true
  @State private var showDeleted = false
  @State private var enrollOrigin: MutationOrigin?
  @State private var createOrigin: MutationOrigin?

  private static let scopedPages: [SidebarItem] = [.users, .devices, .transfers]
  private var scopeAvailability: [[UUID]] {
    Self.scopedPages.map { model.scopeServers(for: $0).map(\.id) }
  }
  private var readOnlyHosts: [ServerProfile] { model.settings.servers.filter(\.isReadOnly) }

  var body: some View {
    presentedSplitView
      .onChange(of: scopeAvailability) { _, _ in syncSelectedServer(for: selection) }
      .onChange(of: readOnlyHosts.map(\.id)) { _, ids in
        if case .host(let id) = selection, !ids.contains(id) { selection = .overview }
      }
      .onChange(of: model.requestOrigin) { _, origin in
        // Enrollment reviews against the selected server; a different selection closes it.
        if enrollOrigin != nil && enrollOrigin != origin { enrollOrigin = nil }
      }
  }

  /// Single-server pages keep using the app's selected server, chosen with the
  /// picker in the page. Opening one selects a server that page can show; a
  /// read-only host page selects that host. Pages spanning every server leave
  /// the selection alone.
  private func syncSelectedServer(for page: SidebarItem?) {
    switch page {
    case .host(let id):
      if model.selectedServerId != id { model.selectServer(id) }
    case .some(let scoped) where Self.scopedPages.contains(scoped):
      let servers = model.scopeServers(for: scoped)
      if servers.isEmpty {
        selection = .overview
      } else if !servers.contains(where: { $0.id == model.selectedServerId }) {
        model.selectServer(servers[0].id)
      }
    default: break
    }
  }

  private var presentedSplitView: some View {
    splitView
      .toolbar {
        MainToolbar(
          selection: selection ?? .overview, showCompleted: $showCompleted,
          showDeleted: $showDeleted,
          createOnServer: { id in
            createOrigin = model.origin(forServer: id)
          },
          enrollOnServer: { id in
            model.selectServer(id)
            enrollOrigin = model.origin(forServer: id)
          })
      }
      .sheet(item: $createOrigin) { origin in CreateVmSheet(origin: origin) }
      .sheet(item: $enrollOrigin) { origin in EnrollmentSheet(origin: origin) }
      .onChange(of: selection) { _, page in
        model.fleetPageActive = (page ?? .overview).spansAllServers
        syncSelectedServer(for: page)
      }
      .onAppear {
        model.fleetPageActive = (selection ?? .overview).spansAllServers
        syncSelectedServer(for: selection)
      }
  }

  private var isFleetPage: Bool { (selection ?? .overview).spansAllServers }
  private var scopedViewIdentity: MutationOrigin? {
    isFleetPage ? nil : model.requestOrigin
  }
  private var pageSubtitle: String {
    isFleetPage ? "" : model.selectedServer?.name ?? ""
  }

  private var splitView: some View {
    NavigationSplitView {
      sidebar
    } detail: {
      // Bound the shared header and page so intrinsic content sizes cannot
      // expand the split layout beyond the window.
      GeometryReader { geometry in
        VStack(spacing: 0) {
          if hasStatusMessages {
            StatusStrip(global: model.fleetPageActive)
              .padding(.horizontal, 20)
              .padding(.top, 20)
              .padding(.bottom, 16)
          }
          detail
            .id(scopedViewIdentity)
            .navigationSubtitle(pageSubtitle)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: geometry.size.width, height: geometry.size.height, alignment: .topLeading)
      }
      .overlay(alignment: .bottomTrailing) { NoticeStack() }
    }
  }

  private var sidebar: some View {
    List(selection: $selection) {
      Section("Cloud") {
        Label("Servers", systemImage: "server.rack")
          .tag(SidebarItem.overview)
        Label("Virtual Machines", systemImage: "desktopcomputer")
          .badge(model.fleetVmCount)
          .tag(SidebarItem.machines)
        Label("Operations", systemImage: "list.bullet.rectangle")
          .badge(model.activeOperationCount)
          .tag(SidebarItem.operations)
        if !model.scopeServers(for: .transfers).isEmpty {
          Label("Transfers", systemImage: "arrow.left.arrow.right").tag(SidebarItem.transfers)
        }
        Label("Pending Requests", systemImage: "tray.full")
          .badge(model.unresolvedCount)
          .tag(SidebarItem.pending)
      }
      if !readOnlyHosts.isEmpty {
        Section("Read-only Hosts") {
          ForEach(readOnlyHosts) { host in
            Label(host.name, systemImage: "eye").lineLimit(1).tag(SidebarItem.host(host.id))
          }
        }
      }
      if !model.scopeServers(for: .users).isEmpty {
        Section("Administration") {
          Label("Users", systemImage: "person.2")
            .tag(SidebarItem.users)
          if !model.scopeServers(for: .devices).isEmpty {
            Label("Devices", systemImage: "laptopcomputer.and.iphone")
              .tag(SidebarItem.devices)
          }
        }
      }
    }
    .navigationSplitViewColumnWidth(min: 200, ideal: 220)
    .safeAreaInset(edge: .bottom) {
      IdentityFooter(global: isFleetPage)
    }
  }

  @ViewBuilder
  private var detail: some View {
    switch selection ?? .overview {
    case .host(let id):
      if model.selectedServerId != id {
        ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
      } else if !model.isConfigured {
        NotConfiguredView(issues: model.configurationIssues)
      } else {
        ObservedHostView(machinesOnly: false)
      }
    case .overview: ServersView()
    case .machines: FleetVirtualMachinesView(showDeleted: $showDeleted)
    case .operations: OperationsView(showCompleted: $showCompleted)
    case .pending: PendingRequestsView()
    case .transfers:
      ServerScopedPage(servers: model.scopeServers(for: .transfers)) { MigrationTransfersView() }
    case .users:
      ServerScopedPage(servers: model.scopeServers(for: .users)) {
        AdminUsersView(origin: model.requestOrigin)
      }
    case .devices:
      ServerScopedPage(servers: model.scopeServers(for: .devices)) {
        AdminDevicesView(origin: model.requestOrigin)
      }
    }
  }

  private var hasStatusMessages: Bool {
    let fleet = model.fleetPageActive
    return (!fleet && model.isReadOnly)
      || (!fleet && model.refreshFailure != nil
        && (model.dashboard != nil || model.observation != nil))
      || (!fleet && model.selectedServerId.map(model.autoRefreshPaused) == true)
      || model.ledgerStorageIssue != nil
      || (fleet ? model.ledger.entries : model.pendingForCurrentProfile).contains {
        $0.phase != .rejected && !$0.isInFlight
      }
  }
}

/// A page that works on one controller at a time. Its in-page picker chooses
/// the app's selected server among the controllers this page can show; changing
/// it rebuilds the page so no draft or review carries over to another server.
struct ServerScopedPage<Content: View>: View {
  @Environment(AppModel.self) private var model
  let servers: [ServerProfile]
  @ViewBuilder let content: () -> Content

  var body: some View {
    VStack(alignment: .leading, spacing: 12) {
      if servers.count > 1 || !servers.contains(where: { $0.id == model.selectedServerId }) {
        HStack {
          Picker(
            "Server",
            selection: Binding(
              get: { model.selectedServerId },
              set: { if let id = $0 { model.selectServer(id) } })
          ) {
            ForEach(servers) { Text($0.name).tag(UUID?.some($0.id)) }
          }
          .fixedSize()
          .accessibilityIdentifier("selected-server")
          Spacer()
        }
        .padding(.horizontal, 20)
      } else if let server = servers.first {
        Label(server.name, systemImage: "server.rack")
          .font(.callout.weight(.medium))
          .foregroundStyle(.secondary)
          .padding(.horizontal, 20)
      }
      if servers.isEmpty {
        ContentUnavailableView(
          "Administrator access required", systemImage: "lock",
          description: Text(
            "None of your servers lists you as an administrator for this page. The server decides permissions."))
          .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else if !servers.contains(where: { $0.id == model.selectedServerId }) {
        ContentUnavailableView(
          "Choose a server", systemImage: "server.rack",
          description: Text("Pick the server this page should show."))
          .frame(maxWidth: .infinity, maxHeight: .infinity)
      } else if !model.isConfigured {
        NotConfiguredView(issues: model.configurationIssues)
      } else {
        content()
      }
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
  }
}

struct IdentityFooter: View {
  @Environment(AppModel.self) private var model
  var global = false

  var body: some View {
    VStack(alignment: .leading, spacing: 4) {
      Divider()
      if global {
        Label("\(model.settings.servers.count) servers", systemImage: "server.rack")
          .font(.caption.weight(.medium))
      } else if let server = model.selectedServer {
        Label(server.name, systemImage: "server.rack")
          .font(.caption.weight(.medium)).lineLimit(1)
      }
      if global {
        ForEach(model.settings.servers.filter { !$0.isReadOnly }) { server in
          Text("\(server.name) · \(model.freshness(server.id))").font(.caption).foregroundStyle(
            .secondary)
        }
      } else if model.isReadOnly {
        Label("Read-only observations", systemImage: "eye")
          .font(.callout).foregroundStyle(.secondary)
      } else if let identity = model.dashboard?.identity {
        HStack(spacing: 6) {
          Image(systemName: "person.crop.circle")
          Text(identity.userId).font(.callout.weight(.medium)).lineLimit(1)
          Spacer(minLength: 0)
          RoleBadge(role: identity.role)
        }
      } else {
        Text("Not connected").font(.callout).foregroundStyle(.secondary)
      }
      if !global, let refreshed = model.lastRefreshedAt {
        Text(
          "\(model.isReadOnly ? "Refreshed" : "Updated") \(refreshed.formatted(date: .omitted, time: .standard))"
        )
        .font(.caption)
        .foregroundStyle(.secondary)
      }
    }
    .padding(.horizontal, 12)
    .padding(.bottom, 10)
  }
}

struct MainToolbar: ToolbarContent {
  @Environment(AppModel.self) private var model
  let selection: SidebarItem
  @Binding var showCompleted: Bool
  @Binding var showDeleted: Bool
  let createOnServer: (UUID) -> Void
  let enrollOnServer: (UUID) -> Void

  var body: some ToolbarContent {
    ToolbarItemGroup(placement: .primaryAction) {
      if selection == .operations {
        Toggle("Show Completed", isOn: $showCompleted)
          .toggleStyle(.checkbox)
          .fixedSize()
          .padding(.leading, 12)
          .padding(.trailing, 14)
      }
      if selection == .machines {
        Toggle("Show Deleted", isOn: $showDeleted)
          .toggleStyle(.checkbox)
          .fixedSize()
          .padding(.leading, 12)
          .padding(.trailing, 14)
        if model.fleetServers.contains(where: { model.enrollVmIssue(forServer: $0.id) == nil }) {
          Menu {
            ForEach(model.fleetServers) { server in
              let issue = model.enrollVmIssue(forServer: server.id)
              Button {
                enrollOnServer(server.id)
              } label: {
                Text(server.name)
                if let issue { Text(issue) }
              }
              .disabled(issue != nil)
              .help(issue ?? "Review and enroll an existing VM permitted by \(server.name)")
            }
          } label: {
            Label("Enroll VM…", systemImage: "tray.and.arrow.down")
          }
          .help("Choose the server whose existing VM to review and enroll")
        }
        Menu {
          ForEach(model.fleetServers) { server in
            let issue = model.createVmIssue(forServer: server.id)
            Button {
              createOnServer(server.id)
            } label: {
              Text(server.name)
              if let issue { Text(issue) }
            }
            .disabled(issue != nil)
            .help(issue ?? "Create a VM on \(server.name)")
          }
        } label: {
          Label("New VM", systemImage: "plus")
        }
        .help("Choose a server for the new VM. Unavailable servers explain why.")
      }
      RefreshButton(fleet: selection.spansAllServers, inventory: selection == .overview)
    }
  }
}

private struct RefreshButton: View {
  @Environment(AppModel.self) private var model
  @State private var showProgress = false
  var fleet = false
  var inventory = false
  private var refreshing: Bool { fleet ? model.fleetIsRefreshing : model.isRefreshing }

  var body: some View {
    Button {
      if refreshing {
        if fleet { model.cancelAllRefreshes() } else { model.cancelRefresh() }
      } else if inventory {
        model.refreshServers()
      } else if fleet {
        model.refreshAll()
      } else {
        model.refresh()
      }
    } label: {
      ZStack {
        Image(systemName: "arrow.clockwise")
          .opacity(showProgress ? 0 : 1)
        if showProgress {
          ProgressView()
            .controlSize(.small)
            .allowsHitTesting(false)
            .accessibilityHidden(true)
        }
      }
      .frame(width: 18, height: 18)
    }
    .disabled(!model.isConfigured && !fleet)
    .accessibilityLabel(refreshing ? "Cancel Refresh" : "Refresh")
    .help(
      refreshing
        ? "Cancel queued and active refresh requests; accepted server work continues."
        : "Refresh this page (⌘R)"
    )
    .task(id: refreshing) {
      showProgress = false
      guard refreshing else { return }
      do {
        try await Task.sleep(for: .milliseconds(300))
        try Task.checkCancellation()
        showProgress = true
      } catch {
        // Fast refreshes and cancelled tasks never show the indicator.
      }
    }
  }
}

extension MutationOrigin: Identifiable {
  var id: String { "\(serverId)/\(revision)" }
}
