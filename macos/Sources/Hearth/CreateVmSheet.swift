import HearthCore
import SwiftUI

struct CreateVmSheet: View {
  let origin: MutationOrigin?
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  @State private var draft = CreateVmDraft()
  @State private var issues: [String] = []
  @State private var catalog: ImageCatalog?
  @State private var loadingCatalog = false
  @State private var catalogFailure: String?

  private var dashboard: DashboardResult? {
    origin.flatMap { model.dashboard(forServer: $0.serverId) }
  }
  private var permissions: Permissions { Permissions(identity: dashboard?.identity) }
  private var managed: Bool { dashboard?.backend == .managed }
  private var selectedImage: ApprovedImage? {
    catalog?.images.first { $0.imageSha256 == draft.imageSha256 }
  }

  var body: some View {
    let budget = dashboard?.budget
    let targetOwner = draft.owner.isEmpty ? permissions.userId ?? "" : draft.owner
    let allocation = dashboard?.storageAllocations.first {
      $0.owner == targetOwner && $0.acceptsNewVms
    }
    let ownerHasRecovery =
      dashboard?.diskShrinks.contains {
        $0.owner == (draft.owner.isEmpty ? permissions.userId ?? "" : draft.owner)
      } == true
    VStack(alignment: .leading, spacing: 14) {
      Text("New Virtual Machine").font(.title2.weight(.semibold))
      LabeledContent("Server", value: model.serverName(on: origin))
      Text(
        managed
          ? "Create a bootable guest from this server’s approved image catalog."
          : "The VM is created with a blank root disk in the selected controller’s lab storage."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      Form {
        TextField("Name", text: $draft.name, prompt: Text("dev-frank"))
        if managed {
          Picker("Boot image", selection: $draft.imageSha256) {
            Text("Choose an approved image").tag("")
            ForEach(catalog?.images ?? []) { image in
              Text(image.displayLabel).tag(image.imageSha256)
            }
          }
          if let image = selectedImage {
            Text(image.accessLabel)
            Text("Minimum disk: \(image.minDiskMib) MiB")
              .foregroundStyle(.secondary)
          }
          TextField(
            "Ed25519 public key", text: $draft.sshPublicKey, prompt: Text("ssh-ed25519 AAAA…")
          )
          .autocorrectionDisabled()
          .accessibilityIdentifier("guest-ssh-public-key")
        }
        TextField("Disk (MiB)", text: $draft.diskMib)
        TextField("Memory (MiB)", text: $draft.memoryMib)
        TextField("Minimum memory (MiB)", text: $draft.minMemoryMib)
        TextField("Cores", text: $draft.cores)
        if permissions.canChooseVmOwner {
          Picker("Owner", selection: $draft.owner) {
            Text("Myself").tag("")
            ForEach((dashboard?.users ?? []).filter { $0.state == .active && $0.ready }) { user in
              if user.userId != permissions.userId {
                Text(user.userId).tag(user.userId)
              }
            }
          }
        }
      }
      .formStyle(.grouped)
      if let allocation {
        Text("Allocation: \(allocation.dataset) · \(Formatting.mib(allocation.budgetMib)) budget")
          .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
      }
      if managed {
        HStack {
          if loadingCatalog {
            ProgressView().controlSize(.small)
            Text("Loading approved images…")
          } else if let catalogFailure {
            Text(catalogFailure).foregroundStyle(.red)
          } else if catalog?.images.isEmpty == true {
            Text(
              "No approved boot images are available on this server. Ask its administrator to configure an image."
            )
          }
          Button("Reload Images") { Task { await loadCatalog() } }.disabled(loadingCatalog)
        }.font(.caption)
      }
      if let budget, draft.owner.isEmpty {
        Text(
          "Your disk budget: \(Formatting.mib(budget.remainingMib)) remaining of \(Formatting.mib(budget.budgetMib))."
        )
        .font(.caption)
        .foregroundStyle(.secondary)
      }
      IssueList(messages: issues)
      if ownerHasRecovery {
        Text("Finish this owner's disk shrink before creating another VM.").font(.callout)
          .foregroundStyle(.secondary)
      }
      HStack {
        Spacer()
        Button("Cancel", role: .cancel) { dismiss() }
          .keyboardShortcut(.cancelAction)
        Button("Create VM") { create() }
          .keyboardShortcut(.defaultAction)
          .disabled(
            ownerHasRecovery || origin != origin.flatMap { model.origin(forServer: $0.serverId) }
              || (managed && (loadingCatalog || selectedImage == nil || catalogFailure != nil)))
      }
    }
    .padding(20)
    .frame(width: 540)
    .task {
      if managed {
        draft.diskMib = "8192"
        draft.memoryMib = "1024"
        draft.minMemoryMib = "512"
        await loadCatalog()
      }
    }
  }

  private func loadCatalog() async {
    loadingCatalog = true
    catalogFailure = nil
    catalog = nil
    draft.imageSha256 = ""
    defer { loadingCatalog = false }
    do { catalog = try await model.loadImageCatalog(origin: origin) } catch {
      catalogFailure = error.detail
    }
  }

  private func create() {
    switch draft.build(
      requestId: UUID(), allowOwner: permissions.canChooseVmOwner, requireGuest: managed,
      catalog: catalog)
    {
    case .failure(let failure):
      issues = failure.messages
    case .success(let request):
      if model.submit(request, origin: origin) != nil {
        dismiss()
      }
    }
  }
}
