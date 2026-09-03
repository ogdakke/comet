import XCTest
@testable import Zeron

@MainActor
final class StudioViewerPerformanceTests: XCTestCase {
    func testThumbhashCreatesAnImmediateAspectCorrectPlaceholder() throws {
        let decoded = StudioThumbHash.image(
            base64: "3OcRJYB4d3h/iIeHeEh3eIhw+j3A",
            aspectRatio: 2.0 / 3.0
        )

        let image = try XCTUnwrap(decoded)
        XCTAssertEqual(image.size.width / image.size.height, 2.0 / 3.0, accuracy: 0.02)
        XCTAssertNil(StudioThumbHash.image(base64: "not-a-thumbhash"))
    }

    func testPreviewDiskCacheRoundTripsCompressedPreviewData() async {
        let deviceId = "test-device-\(UUID().uuidString)"
        let artifactId = "test-artifact-\(UUID().uuidString)"
        let bytes = Data("preview-data".utf8)

        await StudioPreviewDiskCache.shared.store(bytes, deviceId: deviceId, artifactId: artifactId)
        let loaded = await StudioPreviewDiskCache.shared.data(deviceId: deviceId, artifactId: artifactId)
        XCTAssertEqual(loaded, bytes)
        XCTAssertEqual(StudioPreviewDiskCache.maximumBytes, 512 * 1024 * 1024)

        await StudioPreviewDiskCache.shared.remove(deviceId: deviceId, artifactId: artifactId)
        let removed = await StudioPreviewDiskCache.shared.data(deviceId: deviceId, artifactId: artifactId)
        XCTAssertNil(removed)
    }

    func testThreadPresentationUsesTheVisibleNavigationDestination() {
        let first = UIViewController()
        let thread = UIViewController()
        let navigation = UINavigationController(rootViewController: first)
        navigation.setViewControllers([first, thread], animated: false)
        XCTAssertTrue(StudioViewerPresentationHost.visibleController(from: navigation) === thread)
    }

    func testThousandItemViewerKeepsOnlyVisibleCellsResident() {
        let artifacts = makeArtifacts(count: 1_000)
        let session = StudioViewerSession(
            artifacts: artifacts,
            selectedId: artifacts[500].id,
            openedFromGallery: false
        )
        let controller = StudioGalleryViewerController(
            session: session,
            browser: StudioBrowserStore(),
            workspace: nil,
            deviceId: nil,
            requestDismissal: {},
            requestThread: { _ in }
        )

        controller.loadViewIfNeeded()
        controller.view.frame = CGRect(x: 0, y: 0, width: 393, height: 852)
        controller.view.setNeedsLayout()
        controller.view.layoutIfNeeded()

        XCTAssertLessThanOrEqual(controller.residentPageCellCountForTesting, 3)
        XCTAssertLessThanOrEqual(controller.residentFilmstripCellCountForTesting, 12)
        XCTAssertEqual(controller.selectedArtifactIdForTesting, artifacts[500].id)
    }

    func testFilmstripCenterUpdatesViewerBeforeSettling() {
        let artifacts = makeArtifacts(count: 1_000)
        let session = StudioViewerSession(
            artifacts: artifacts,
            selectedId: artifacts[500].id,
            openedFromGallery: false
        )
        let controller = StudioGalleryViewerController(
            session: session,
            browser: StudioBrowserStore(),
            workspace: nil,
            deviceId: nil,
            requestDismissal: {},
            requestThread: { _ in }
        )
        controller.loadViewIfNeeded()
        controller.view.frame = CGRect(x: 0, y: 0, width: 393, height: 852)
        controller.view.layoutIfNeeded()

        controller.centerFilmstripItemUsingCollectionGeometryForTesting(at: 527)

        XCTAssertEqual(controller.selectedArtifactIdForTesting, artifacts[527].id)
        XCTAssertEqual(session.selectedId, artifacts[527].id)
    }

    func testSnapResolverUsesImagePeekThenFreeScroll() {
        let peek: CGFloat = 220

        XCTAssertEqual(
            StudioViewerSnapResolver.targetOffset(
                current: 12,
                proposed: 600,
                velocity: 1.8,
                peek: peek
            ),
            peek,
            "the first upward fling stops at the details peek"
        )
        XCTAssertEqual(
            StudioViewerSnapResolver.targetOffset(
                current: peek,
                proposed: 620,
                velocity: 1.4,
                peek: peek
            ),
            620,
            "an upward fling from the peek enters free scrolling"
        )
        XCTAssertEqual(
            StudioViewerSnapResolver.targetOffset(
                current: peek,
                proposed: 40,
                velocity: -1.1,
                peek: peek
            ),
            0,
            "a downward fling from the peek returns to the image"
        )
        XCTAssertEqual(
            StudioViewerSnapResolver.targetOffset(
                current: 520,
                proposed: 180,
                velocity: -1.2,
                peek: peek
            ),
            peek,
            "free scrolling returns through the peek instead of overshooting"
        )
    }

    private func makeArtifacts(count: Int) -> [StudioArtifactDetail] {
        (0..<count).map { index in
            StudioArtifactDetail(item: StudioGalleryItem(
                id: "performance-\(index)",
                conversationId: "thread-\(index / 8)",
                turnId: "turn-\(index / 4)",
                outputPosition: UInt32(index % 4),
                mediaKind: .image,
                mimeType: "image/jpeg",
                sizeBytes: 2_400_000,
                width: 1_024,
                height: 1_536,
                prompt: "Prompt \(index)",
                modelDisplayName: "Model",
                createdAt: "2026-08-24T18:00:00Z",
                thumbhash: nil,
                sourceArtifactId: nil,
                durationSeconds: nil
            ))
        }
    }
}
