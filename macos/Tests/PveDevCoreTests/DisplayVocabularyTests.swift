import Foundation
import Testing

@testable import PveDevCore

@Suite("Canonical display vocabulary")
struct DisplayVocabularyTests {
  @Test func blockedStatesReadAsNeedsInvestigation() {
    #expect(DisplayVocabulary.vmState(.blocked) == "Needs investigation")
    #expect(DisplayVocabulary.operationStatus(.blocked) == "Needs investigation")
    #expect(DisplayVocabulary.migrationPhase("blocked") == "Needs investigation")
  }

  @Test func successfulOutcomesReadAsCompleted() {
    #expect(DisplayVocabulary.operationStatus(.verified) == "Completed")
    #expect(DisplayVocabulary.migrationPhase("complete") == "Completed")
    #expect(DisplayVocabulary.migrationPhase("aborted") == "Cancelled")
  }

  @Test func ownerIsTheOnlyOwnershipLabel() {
    #expect(DisplayVocabulary.owner == "Owner")
  }

  @Test func wireValuesAreUnchanged() throws {
    #expect(OperationStatus.verified.rawValue == "verified")
    #expect(OperationStatus.blocked.rawValue == "blocked")
    #expect(VmState.blocked.rawValue == "blocked")
    let decoded = try JSONDecoder().decode([OperationStatus].self, from: Data(#"["verified","blocked"]"#.utf8))
    #expect(decoded == [.verified, .blocked])
    let encoded = String(decoding: try JSONEncoder().encode(VmState.blocked), as: UTF8.self)
    #expect(encoded == #""blocked""#)
  }

  @Test func rawMigrationPhasesBecomeReadable() {
    #expect(DisplayVocabulary.migrationProgress(role: "source", phase: "relinquished")
      == "Source · Handed off to destination")
    #expect(DisplayVocabulary.migrationPhase("fenced") == "Source locked for transfer")
    #expect(DisplayVocabulary.migrationProgress(role: "destination", phase: "receiving")
      == "Destination · Copying disk data")
    for phase in ["authorized", "preparing", "fenced", "receiving", "ready", "relinquished", "accepted", "cleaning"] {
      #expect(DisplayVocabulary.migrationPhase(phase) != phase)
      #expect(!DisplayVocabulary.migrationPhase(phase).contains("_"))
    }
  }

  @Test func unknownValuesAreSanitizedAndSentenceCased() {
    #expect(DisplayVocabulary.migrationPhase("awaiting_peer") == "Awaiting peer")
    #expect(DisplayVocabulary.vmState(VmState(rawValue: "paused")) == "Paused")
    #expect(DisplayVocabulary.operationStatus(OperationStatus(rawValue: "")) == "Unknown")
    #expect(DisplayVocabulary.migrationRole("observer") == "Observer")
  }

  @Test func backendsAreNamedDistinctly() {
    #expect(DisplayVocabulary.backend(.lab) == "Lab")
    #expect(DisplayVocabulary.backend(.physicalTrial) == "Physical trial")
    #expect(DisplayVocabulary.backend(.lab) != DisplayVocabulary.backend(.physicalTrial))
  }
}
