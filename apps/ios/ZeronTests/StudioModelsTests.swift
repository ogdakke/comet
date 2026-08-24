import XCTest
@testable import Zeron

final class StudioModelsTests: XCTestCase {
    func testDecodesThreadSummaryFromEngineWireShape() throws {
        let data = Data(#"""
        {
          "id":"thread-1","title":"Night harbor","turnCount":2,
          "createdAt":"2026-08-24T18:00:00Z","updatedAt":"2026-08-24T18:02:03.456Z",
          "archived":false,"creating":true,"done":false
        }
        """#.utf8)

        let summary = try JSONDecoder().decode(StudioThreadSummary.self, from: data)

        XCTAssertEqual(summary.id, "thread-1")
        XCTAssertEqual(summary.turnCount, 2)
        XCTAssertTrue(summary.creating)
        XCTAssertGreaterThan(summary.updatedDate, .distantPast)
    }

    func testDecodesMinimalConversationProjection() throws {
        let data = Data(#"""
        {
          "conversation":{
            "id":"thread-1","title":"Night harbor","turnCount":1,
            "createdAt":"2026-08-24T18:00:00Z","updatedAt":"2026-08-24T18:02:00Z",
            "archived":false,"creating":false,"done":true
          },
          "turns":[{
            "id":"turn-1","position":0,"prompt":"A quiet harbor at night",
            "sourceTurnId":null,"batchId":"batch-1","createdAt":"2026-08-24T18:00:00Z",
            "runs":[{
              "id":"run-1","position":0,"providerId":"venice",
              "model":{"id":"flux","display_name":"Flux"},
              "state":"succeeded","progress":null,"error":null,
              "artifacts":[{
                "id":"artifact-1","outputPosition":0,"mediaKind":"image",
                "mimeType":"image/jpeg","sizeBytes":1024,"width":1024,"height":1024,
                "durationSeconds":null,"metadata":{},"createdAt":"2026-08-24T18:01:00Z",
                "thumbhash":"AQID","contentHash":"abc"
              }]
            }]
          }]
        }
        """#.utf8)

        let thread = try JSONDecoder().decode(StudioThread.self, from: data)

        XCTAssertEqual(thread.turns.first?.runs.first?.model.displayName, "Flux")
        XCTAssertEqual(thread.turns.first?.runs.first?.artifacts.first?.aspectRatio, 1)
    }

    @MainActor
    func testViewerSessionAppendsLargeGalleryPagesWithoutDuplicates() {
        let first = (0..<60).map(galleryItem)
        let session = StudioViewerSession(
            artifacts: first.map(StudioArtifactDetail.init(item:)),
            selectedId: first[59].id,
            openedFromGallery: true
        )
        let remaining = (40..<1_100).map(galleryItem)

        session.append(remaining.map(StudioArtifactDetail.init(item:)))

        XCTAssertEqual(session.artifacts.count, 1_100)
        XCTAssertEqual(Set(session.artifacts.map(\.id)).count, 1_100)
        XCTAssertEqual(session.selected?.id, "artifact-59")
    }

    @MainActor
    func testViewerSessionAdoptsInvertedGalleryOrderWithoutLosingSelection() {
        let initial = (0..<3).map(galleryItem).map(StudioArtifactDetail.init(item:))
        let session = StudioViewerSession(
            artifacts: initial,
            selectedId: "artifact-1",
            openedFromGallery: true
        )
        let inverted = [galleryItem(4), galleryItem(3), galleryItem(2), galleryItem(1), galleryItem(0)]
            .map(StudioArtifactDetail.init(item:))

        session.replaceArtifacts(with: inverted)

        XCTAssertEqual(session.artifacts.map(\.id), [
            "artifact-4", "artifact-3", "artifact-2", "artifact-1", "artifact-0",
        ])
        XCTAssertEqual(session.selected?.id, "artifact-1")
    }

    private func galleryItem(_ index: Int) -> StudioGalleryItem {
        StudioGalleryItem(
            id: "artifact-\(index)",
            conversationId: "thread-\(index / 4)",
            turnId: "turn-\(index / 4)",
            outputPosition: UInt32(index % 4),
            mediaKind: .image,
            mimeType: "image/jpeg",
            sizeBytes: 1_024,
            width: 1_024,
            height: 1_024,
            prompt: "Prompt \(index)",
            modelDisplayName: "Model",
            createdAt: "2026-08-24T18:00:00Z",
            thumbhash: nil,
            sourceArtifactId: nil,
            durationSeconds: nil
        )
    }
}
