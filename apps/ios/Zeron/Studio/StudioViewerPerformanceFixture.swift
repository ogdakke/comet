#if DEBUG
import SwiftUI
import UIKit

/// Explicit simulator-only harness for profiling the real viewer controller
/// with a four-digit item count and no relay or authentication dependency.
struct StudioViewerPerformanceFixture: UIViewControllerRepresentable {
    func makeUIViewController(context: Context) -> StudioViewerFixtureHostController {
        StudioViewerFixtureHostController()
    }

    func updateUIViewController(
        _ uiViewController: StudioViewerFixtureHostController,
        context: Context
    ) {}
}

@MainActor
final class StudioViewerFixtureHostController: UIViewController, StudioGalleryTransitionSourceProvider {
    private let browser = StudioBrowserStore()
    private let transitionSource = StudioGalleryTransitionSource()
    private let frameMeter = StudioFixtureFrameMeter()
    private var artifacts: [StudioArtifactDetail] = []
    private var sourceViews: [String: UIImageView] = [:]
    private var didOpen = false
    private var profiling = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = UIColor(Theme.bg)
        buildFixture()
        buildBackgroundGallery()
        transitionSource.provider = self

        var configuration = UIButton.Configuration.filled()
        configuration.title = "Open 1,000-item viewer"
        let button = UIButton(configuration: configuration)
        button.addAction(UIAction { [weak self] _ in self?.openViewer() }, for: .touchUpInside)
        button.frame = CGRect(x: 56, y: view.bounds.height - 110, width: view.bounds.width - 112, height: 48)
        button.autoresizingMask = [.flexibleWidth, .flexibleTopMargin]
        view.addSubview(button)
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        if profiling, presentedViewController == nil {
            finishProfiling()
            return
        }
        guard !didOpen else { return }
        didOpen = true
        if ProcessInfo.processInfo.arguments.contains("-studio-viewer-autoplay") {
            profiling = true
            frameMeter.start()
        }
        openViewer()
    }

    func transitionImageView(for artifactId: String) -> UIView? {
        sourceViews[artifactId]
    }

    private func buildFixture() {
        let base = Date(timeIntervalSince1970: 1_788_000_000)
        let formatter = ISO8601DateFormatter()
        artifacts = (0..<1_000).map { index in
            StudioArtifactDetail(item: StudioGalleryItem(
                id: "fixture-\(index)",
                conversationId: "fixture-thread-\(index / 8)",
                turnId: "fixture-turn-\(index / 4)",
                outputPosition: UInt32(index % 4),
                mediaKind: .image,
                mimeType: "image/jpeg",
                sizeBytes: 2_400_000,
                width: index.isMultiple(of: 3) ? 1_536 : 1_024,
                height: index.isMultiple(of: 3) ? 1_024 : 1_536,
                prompt: "Performance fixture image \(index). This prompt exercises details layout without any backend work.",
                modelDisplayName: "Fixture",
                createdAt: formatter.string(from: base.addingTimeInterval(Double(index))),
                thumbhash: nil,
                sourceArtifactId: nil,
                durationSeconds: nil
            ))
        }

        let palette = (0..<12).map {
            Self.image(index: 440 + $0, size: CGSize(width: 180, height: 240))
        }
        for index in 440...560 {
            browser.seedPreviewForPerformanceFixture(
                palette[index % palette.count],
                artifactId: artifacts[index].id
            )
        }
    }

    private func buildBackgroundGallery() {
        let columns = 3
        let spacing: CGFloat = 2
        let side = (view.bounds.width - CGFloat(columns - 1) * spacing) / CGFloat(columns)
        let palette = (0..<9).map {
            Self.image(index: 470 + $0, size: CGSize(width: 120, height: 120))
        }
        for index in 0..<30 {
            let imageView = UIImageView(image: palette[index % palette.count])
            imageView.contentMode = .scaleAspectFill
            imageView.clipsToBounds = true
            let row = index / columns
            let column = index % columns
            imageView.frame = CGRect(
                x: CGFloat(column) * (side + spacing),
                y: CGFloat(row) * (side + spacing),
                width: side,
                height: side
            )
            view.addSubview(imageView)
            sourceViews["fixture-\(470 + index)"] = imageView
        }
    }

    private func openViewer() {
        guard presentedViewController == nil else { return }
        let session = StudioViewerSession(
            artifacts: artifacts,
            selectedId: artifacts[484].id,
            openedFromGallery: true,
            openingPreview: browser.cachedPreview(artifactId: artifacts[484].id)
        )
        let viewer = StudioGalleryViewerController(
            session: session,
            browser: browser,
            workspace: nil,
            deviceId: nil,
            requestDismissal: { [weak self] in
                self?.dismiss(animated: true) { [weak self] in self?.finishProfiling() }
            },
            requestThread: { _ in }
        )
        StudioViewerNativeTransition.configure(viewer, session: session, source: transitionSource)
        viewer.modalPresentationStyle = .fullScreen
        present(viewer, animated: true) {
            if ProcessInfo.processInfo.arguments.contains("-studio-viewer-autoplay") {
                Task { @MainActor in
                    try? await Task.sleep(for: .milliseconds(750))
                    viewer.runPerformanceFixtureSequence()
                }
            }
        }
    }

    private func finishProfiling() {
        guard profiling else { return }
        profiling = false
        frameMeter.stopAndReport()
    }

    private static func image(index: Int, size: CGSize) -> UIImage {
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        return UIGraphicsImageRenderer(size: size, format: format).image { context in
            UIColor(
                hue: CGFloat(index % 41) / 41,
                saturation: 0.55,
                brightness: 0.72,
                alpha: 1
            ).setFill()
            context.fill(CGRect(origin: .zero, size: size))

            let text = "\(index)" as NSString
            let attributes: [NSAttributedString.Key: Any] = [
                .font: UIFont.monospacedDigitSystemFont(ofSize: min(size.width, size.height) * 0.2, weight: .bold),
                .foregroundColor: UIColor.white,
            ]
            let textSize = text.size(withAttributes: attributes)
            text.draw(
                at: CGPoint(x: (size.width - textSize.width) / 2, y: (size.height - textSize.height) / 2),
                withAttributes: attributes
            )
        }
    }
}

@MainActor
private final class StudioFixtureFrameMeter {
    private var displayLink: CADisplayLink?
    private var previousTimestamp: CFTimeInterval?
    private var frames = 0
    private var hitches = 0
    private var severeHitches = 0
    private var maximumGap: CFTimeInterval = 0

    func start() {
        previousTimestamp = nil
        frames = 0
        hitches = 0
        severeHitches = 0
        maximumGap = 0
        let displayLink = CADisplayLink(target: self, selector: #selector(tick(_:)))
        displayLink.add(to: .main, forMode: .common)
        self.displayLink = displayLink
    }

    func stopAndReport() {
        displayLink?.invalidate()
        displayLink = nil
        NSLog(
            "STUDIO_VIEWER_PROFILE frames=\(frames) hitches=\(hitches) "
                + "severe=\(severeHitches) maxGapMs=\(Int((maximumGap * 1_000).rounded()))"
        )
    }

    @objc private func tick(_ displayLink: CADisplayLink) {
        defer {
            previousTimestamp = displayLink.timestamp
            frames += 1
        }
        guard let previousTimestamp else { return }
        let gap = displayLink.timestamp - previousTimestamp
        maximumGap = max(maximumGap, gap)
        if gap > 1.0 / 30.0 { hitches += 1 }
        if gap > 0.1 { severeHitches += 1 }
    }
}
#endif
