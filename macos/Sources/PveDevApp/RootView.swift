import PveDevCore
import SwiftUI

enum SidebarItem: Hashable {
  case overview
  case machines
  case operations
  case transfers
  case pending
  case users
  case devices
}

struct RootView: View {
  @Environment(AppModel.self) private var model
  @State private var selection: SidebarItem? = .overview
  @State private var showVerified = true
  @State private var showDeleted = false
  @State private var showEnrollVm = false
  @State private var createOrigin: MutationOrigin?

  var body: some View {
    presentedSplitView
      .onChange(of: model.permissions.isAdmin) { _, isAdmin in
        if !isAdmin, selection == .users || selection == .devices || selection == .transfers {
          selection = .overview
        }
      }
      .onChange(of: model.dashboard?.capabilities) { _, _ in
        if selection == .transfers && model.dashboard?.supports("migration") != true {
          selection = .overview
        }
      }
      .onChange(of: model.requestOrigin) { _, _ in
        showEnrollVm = false
      }
      .onChange(of: model.isReadOnly) { _, readOnly in
        if readOnly, selection != .overview && selection != .machines { selection = .overview }
      }
  }

  private var presentedSplitView: some View {
    splitView
      .toolbar {
        MainToolbar(
          selection: selection ?? .overview, showVerified: $showVerified,
          showDeleted: $showDeleted, showEnrollVm: $showEnrollVm,
          createOnServer: { id in
            createOrigin = model.origin(forServer: id)
          })
      }
      .sheet(item: $createOrigin) { origin in CreateVmSheet(origin: origin) }
      .onChange(of: selection) { _, page in
        model.fleetPageActive = page == .overview || page == .machines
      }
      .onAppear { model.fleetPageActive = selection == .overview || selection == .machines }
  }

  private var scopedViewIdentity: MutationOrigin? {
    if selection == .overview || (selection == .machines && !model.usesLegacyMachines) {
      return nil
    }
    return model.requestOrigin
  }
  private var pageSubtitle: String {
    if selection == .overview || (selection == .machines && !model.usesLegacyMachines) { return "" }
    return model.selectedServer?.name ?? ""
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
      Section(model.isReadOnly ? "Read-only Host" : "Cloud") {
        Label("Servers", systemImage: "server.rack")
          .tag(SidebarItem.overview)
        Label("Virtual Machines", systemImage: "desktopcomputer")
          .badge(model.isReadOnly ? (model.observation?.vms.count ?? 0) : model.fleetVmCount)
          .tag(SidebarItem.machines)
        if !model.isReadOnly {
          Label("Operations", systemImage: "list.bullet.rectangle")
            .badge(model.activeOperationCount)
            .tag(SidebarItem.operations)
          if model.permissions.isAdmin && model.dashboard?.supports("migration") == true {
            Label("Transfers", systemImage: "arrow.left.arrow.right").tag(SidebarItem.transfers)
          }
          Label("Pending Requests", systemImage: "tray.full")
            .badge(model.unresolvedCount)
            .tag(SidebarItem.pending)
        }
      }
      if model.permissions.isAdmin {
        Section("Administration") {
          Label("Users", systemImage: "person.2")
            .tag(SidebarItem.users)
          if model.dashboard?.backend != .managed {
            Label("Devices", systemImage: "laptopcomputer.and.iphone")
              .tag(SidebarItem.devices)
          }
        }
      }
    }
    .navigationSplitViewColumnWidth(min: 200, ideal: 220)
    .safeAreaInset(edge: .top) {
      if !model.settings.servers.isEmpty
        && (model.isReadOnly || model.usesLegacyMachines
          || (selection != .overview && selection != .machines))
      {
        Picker(
          "Server",
          selection: Binding(
            get: { model.selectedServerId },
            set: { if let id = $0 { model.selectServer(id) } })
        ) {
          ForEach(model.settings.servers) { server in
            Text(server.name).tag(Optional(server.id))
          }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .accessibilityIdentifier("selected-server")
      }
    }
    .safeAreaInset(edge: .bottom) {
      IdentityFooter(global: selection == .overview || selection == .machines)
    }
  }

  @ViewBuilder
  private var detail: some View {
    if model.isReadOnly {
      if !model.isConfigured {
        NotConfiguredView(issues: model.configurationIssues)
      } else {
        ObservedHostView(machinesOnly: selection == .machines)
      }
    } else if selection == .pending {
      PendingRequestsView()
    } else if !model.isConfigured && selection != .overview && selection != .machines {
      NotConfiguredView(issues: model.configurationIssues)
    } else {
      switch selection ?? .overview {
      case .overview: ServersView()
      case .machines:
        if model.dashboard?.backend == .managed || model.dashboard == nil {
          FleetVirtualMachinesView(showDeleted: $showDeleted)
        } else {
          LegacyVirtualMachinesView(
            showDeleted: $showDeleted, showEnroll: $showEnrollVm,
            origin: model.requestOrigin
          ).id(model.requestOrigin)
        }
      case .operations: OperationsView(showVerified: $showVerified)
      case .transfers: MigrationTransfersView()
      case .pending: PendingRequestsView()
      case .users: AdminUsersView(origin: model.requestOrigin)
      case .devices: AdminDevicesView(origin: model.requestOrigin)
      }
    }
  }

  private var hasStatusMessages: Bool {
    model.isReadOnly
      || (!model.fleetPageActive && model.refreshFailure != nil
        && (model.dashboard != nil || model.observation != nil))
      || model.ledgerStorageIssue != nil
      || (model.fleetPageActive ? model.ledger.entries : model.pendingForCurrentProfile).contains {
        $0.phase != .rejected && !$0.isInFlight
      }
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
  @Binding var showVerified: Bool
  @Binding var showDeleted: Bool
  @Binding var showEnrollVm: Bool
  let createOnServer: (UUID) -> Void

  var body: some ToolbarContent {
    ToolbarItemGroup(placement: .primaryAction) {
      if !model.isReadOnly && selection == .operations {
        Toggle("Show Verified", isOn: $showVerified)
          .toggleStyle(.checkbox)
          .fixedSize()
          .padding(.leading, 12)
          .padding(.trailing, 14)
      }
      if !model.isReadOnly && selection == .machines {
        Toggle("Show Deleted", isOn: $showDeleted)
          .toggleStyle(.checkbox)
          .fixedSize()
          .padding(.leading, 12)
          .padding(.trailing, 14)
        if model.permissions.isAdmin && model.dashboard?.supports("plan_vm_enrollment") == true
          && model.dashboard?.supports("enroll_vm") == true
        {
          Button {
            showEnrollVm = true
          } label: {
            Label("Enroll VM…", systemImage: "tray.and.arrow.down")
          }
          .help("Review and enroll an existing VM permitted by this server")
        }
        Menu {
          ForEach(model.settings.servers.filter { !$0.isReadOnly }) { server in
            Button(server.name) { createOnServer(server.id) }
              .disabled(model.dashboard(forServer: server.id)?.canCreateManagedVm != true)
          }
        } label: {
          Label("New VM", systemImage: "plus")
        }
        .help("Choose a physical server for the new VM")
      }
      RefreshButton(
        fleet: selection == .machines || selection == .overview, inventory: selection == .overview)
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
