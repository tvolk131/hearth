import PveDevCore
import SwiftUI

struct OperationInspectionTarget: Identifiable {
  let id = UUID()
  let operation: OperationRecord
  let origin: MutationOrigin?
}

struct OperationInspectionSheet: View {
  @Environment(AppModel.self) private var model
  @Environment(\.dismiss) private var dismiss
  let target: OperationInspectionTarget
  @State private var inspection: OperationInspection?
  @State private var failure: RpcFailure?
  @State private var isLoading = false
  @State private var confirmed = false

  var body: some View {
    VStack(alignment: .leading, spacing: 14) {
      Text("Inspect operation on \(model.serverName(on: target.origin))").font(
        .title2.weight(.semibold))
      Text(OperationPresentation.title(for: target.operation.kind)).font(.headline)
      Text(SafeText.sanitize(target.operation.operationId, limit: 64))
        .font(.caption.monospaced()).textSelection(.enabled)
      if let inspection {
        Text(SafeText.sanitize(inspection.classification, limit: 160)).fontWeight(.medium)
        ScrollView {
          VStack(alignment: .leading, spacing: 12) {
            ForEach(Array(inspection.evidence.prefix(128).enumerated()), id: \.offset) {
              _, evidence in
              VStack(alignment: .leading, spacing: 3) {
                HStack {
                  Text(SafeText.sanitize(evidence.name, limit: 100)).fontWeight(.medium)
                  Spacer()
                  Text(SafeText.sanitize(evidence.status, limit: 40)).foregroundStyle(.secondary)
                }
                Text(SafeText.sanitize(evidence.detail, limit: 1200))
                  .foregroundStyle(.secondary).textSelection(.enabled)
              }
            }
          }
          .frame(maxWidth: .infinity, alignment: .leading)
        }.frame(maxHeight: 300)
        if inspection.offeredActions.isEmpty {
          Text(
            "No recovery action is currently available. The operation and its reserved resources remain held for investigation."
          )
          .foregroundStyle(.secondary)
        } else {
          Toggle(
            "I reviewed this inspection and authorize recovery on \(model.serverName(on: target.origin))",
            isOn: $confirmed)
          ForEach(inspection.offeredActions, id: \.self) { action in
            HStack {
              Text(action.explanation).font(.callout).foregroundStyle(.secondary)
              Spacer()
              Button(action.title, role: action == .abortPreparation ? .destructive : nil) {
                let request = RecoverOperationRequest(
                  requestId: UUID(), operationId: inspection.operation.operationId,
                  inspectionId: inspection.inspectionId, kind: action, confirmRecovery: confirmed)
                if model.submit(.recoverOperation(request), origin: target.origin) != nil {
                  dismiss()
                }
              }
              .disabled(
                !confirmed || isLoading || target.origin != model.requestOrigin
                  || !model.permissions.isAdmin || model.isReadOnly
                  || model.dashboard?.supports("recover_operation") != true)
            }
          }
        }
      }
      if let failure {
        Text(failure.title).fontWeight(.medium)
        Text(failure.detail).foregroundStyle(.red).textSelection(.enabled)
      }
      HStack {
        if isLoading {
          ProgressView().controlSize(.small)
          Text("Inspecting actual server state…").foregroundStyle(.secondary)
        }
        Spacer()
        Button("Inspect Again") { Task { await load() } }.disabled(
          isLoading || target.origin != model.requestOrigin)
        Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
      }
    }
    .padding(24).frame(width: 640)
    .task { await load() }
  }

  @MainActor
  private func load() async {
    guard !isLoading else { return }
    isLoading = true
    failure = nil
    inspection = nil
    confirmed = false
    defer { isLoading = false }
    do throws(RpcFailure) {
      let value = try await model.query(
        .inspectOperation(operationId: target.operation.operationId),
        origin: target.origin, as: OperationInspection.self)
      guard value.operation.operationId == target.operation.operationId else {
        throw .malformedResponse("The inspection belongs to another operation.")
      }
      inspection = value
    } catch { failure = error }
  }
}
