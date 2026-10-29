import QuartzCore
import UIKit

/// Main-thread cost counters for the Studio gallery (read by `-studio-bench`).
@MainActor
enum StudioGalleryPerf {
    static var applies = 0
    static var maxApplyMs = 0.0
    static var totalApplyMs = 0.0
    static var configures = 0
    static var maxConfigureMs = 0.0
    static var mainThumbhashDecodes = 0
    static var maxPageMergeMs = 0.0
    static var maxSnapshotMs = 0.0
    static var maxRelayoutMs = 0.0
    static var maxAnchorDriftPt = 0.0
    /// Page requested (the grid's top reached) → its items in the grid.
    static var pageRequestedAt: CFTimeInterval?
    static var pageLatenciesMs: [Double] = []

    static func reset() {
        applies = 0; maxApplyMs = 0; totalApplyMs = 0
        configures = 0; maxConfigureMs = 0
        mainThumbhashDecodes = 0; maxPageMergeMs = 0; maxSnapshotMs = 0; maxRelayoutMs = 0; maxAnchorDriftPt = 0
        pageRequestedAt = nil; pageLatenciesMs = []
    }

    static func time<T>(_ body: () -> T, record: (Double) -> Void) -> T {
        let start = CACurrentMediaTime()
        let value = body()
        record((CACurrentMediaTime() - start) * 1000)
        return value
    }

    static var json: [String: Any] {
        let pages = pageLatenciesMs.sorted()
        return [
            "applies": applies,
            "maxApplyMs": round2(maxApplyMs),
            "totalApplyMs": round2(totalApplyMs),
            "configures": configures,
            "maxConfigureMs": round2(maxConfigureMs),
            "mainThumbhashDecodes": mainThumbhashDecodes,
            "maxPageMergeMs": round2(maxPageMergeMs),
            "maxSnapshotMs": round2(maxSnapshotMs),
            "maxRelayoutMs": round2(maxRelayoutMs),
            "maxAnchorDriftPt": round2(maxAnchorDriftPt),
            "pages": pages.count,
            "pageLatencyMedianMs": round2(pages.isEmpty ? 0 : pages[pages.count / 2]),
            "pageLatencyMaxMs": round2(pages.last ?? 0),
        ]
    }

    nonisolated static func round2(_ value: Double) -> Double { (value * 100).rounded() / 100 }
}

/// Longest main-thread run-loop pass (work between two waits): what a real
/// device would drop frames on, independent of the simulator's vsync.
@MainActor
final class StudioMainThreadWatch {
    private var observer: CFRunLoopObserver?
    private var started: CFTimeInterval = 0
    private(set) var maxBusyMs = 0.0
    private(set) var over8 = 0
    private(set) var over16 = 0
    private(set) var over33 = 0
    private(set) var worst: [Double] = []

    func start() {
        let observer = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.afterWaiting.rawValue | CFRunLoopActivity.beforeWaiting.rawValue, true, 0) { [weak self] _, activity in
            MainActor.assumeIsolated { self?.note(activity) }
        }
        CFRunLoopAddObserver(CFRunLoopGetMain(), observer, .commonModes)
        self.observer = observer
        started = CACurrentMediaTime()
    }

    func stop() {
        if let observer { CFRunLoopRemoveObserver(CFRunLoopGetMain(), observer, .commonModes) }
        observer = nil
    }

    func reset() {
        maxBusyMs = 0; over8 = 0; over16 = 0; over33 = 0; worst = []
        started = CACurrentMediaTime()
    }

    private func note(_ activity: CFRunLoopActivity) {
        let now = CACurrentMediaTime()
        if activity == .afterWaiting {
            started = now
            return
        }
        let busy = (now - started) * 1000
        maxBusyMs = max(maxBusyMs, busy)
        if busy > 8.3 { over8 += 1 }
        if busy > 16.7 { over16 += 1 }
        if busy > 33.3 { over33 += 1 }
        if busy > 8.3 {
            worst.append(busy)
            if worst.count > 40 { worst.sort(by: >); worst.removeLast(worst.count - 20) }
        }
    }

    var json: [String: Any] {
        [
            "maxBusyMs": StudioGalleryPerf.round2(maxBusyMs),
            "busyOver8ms": over8,
            "busyOver16ms": over16,
            "busyOver33ms": over33,
            "worstBusyMs": worst.sorted(by: >).prefix(8).map(StudioGalleryPerf.round2),
        ]
    }
}

/// `-studio-bench`: display-link flings up through the whole library (paging
/// every older page in) and back down, counting late frames, the longest
/// main-thread passes, and frames where a visible square had no image at all.
/// Writes `Documents/studio-bench.json`.
@MainActor
final class StudioGalleryBench {
    struct Phase {
        var name: String
        var frames = 0
        var hitches = 0
        var hitchMs = 0.0
        var seconds = 0.0
        var worstFrameMs = 0.0
        /// Frames where a visible square showed nothing (no thumbhash, no preview).
        var blankFrames = 0
        var blankTiles = 0
        /// Visible square-frames showing their thumbhash, not yet the preview.
        var placeholderTiles = 0
        var tiles = 0
        /// Frames held at the top waiting for an older page.
        var topStallFrames = 0

        var json: [String: Any] {
            [
                "frames": frames,
                "hitches": hitches,
                "hitchRatioMsPerS": StudioGalleryPerf.round2(seconds > 0 ? hitchMs / seconds : 0),
                "worstFrameMs": StudioGalleryPerf.round2(worstFrameMs),
                "seconds": StudioGalleryPerf.round2(seconds),
                "blankFrames": blankFrames,
                "blankTiles": blankTiles,
                "placeholderTileRatio": StudioGalleryPerf.round2(tiles > 0 ? Double(placeholderTiles) / Double(tiles) : 0),
                "topStallFrames": topStallFrames,
            ]
        }
    }

    private weak var collectionView: UICollectionView?
    private let hasMore: () -> Bool
    private let tileState: (UICollectionViewCell) -> (image: Bool, preview: Bool)?
    private let watch = StudioMainThreadWatch()
    private var link: CADisplayLink?
    private var last: CFTimeInterval = 0
    private var velocity: CGFloat = 0
    private var direction: CGFloat = -1
    private var phases: [Phase] = []
    private var phase = Phase(name: "up")
    private var watches: [[String: Any]] = []
    private var startedAt: CFTimeInterval = 0
    private let label = UILabel()
    private var retained: StudioGalleryBench?

    init(
        collectionView: UICollectionView,
        hasMore: @escaping () -> Bool,
        tileState: @escaping (UICollectionViewCell) -> (image: Bool, preview: Bool)?
    ) {
        self.collectionView = collectionView
        self.hasMore = hasMore
        self.tileState = tileState
    }

    func run() {
        retained = self
        StudioGalleryPerf.reset()
        watch.start()
        startedAt = CACurrentMediaTime()
        fling(-1)
        let link = CADisplayLink(target: self, selector: #selector(tick(_:)))
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 60, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        self.link = link
        NSLog("STUDIO-BENCH start")
    }

    private func fling(_ direction: CGFloat) {
        velocity = 6000 // pt/s, a hard flick
        self.direction = direction
    }

    @objc private func tick(_ link: CADisplayLink) {
        guard let collectionView else { return link.invalidate() }
        defer { last = link.timestamp }
        guard last > 0 else { return }
        let dt = link.timestamp - last
        let budget = link.targetTimestamp - link.timestamp
        phase.frames += 1
        phase.seconds += dt
        let late = dt - budget
        if late > budget * 0.5 {
            phase.hitches += 1
            phase.hitchMs += late * 1000
        }
        phase.worstFrameMs = max(phase.worstFrameMs, dt * 1000)
        sampleTiles(collectionView)

        let top = -collectionView.adjustedContentInset.top
        let bottom = max(top, collectionView.contentSize.height + collectionView.adjustedContentInset.bottom - collectionView.bounds.height)
        velocity *= CGFloat(pow(0.998, dt * 1000))
        var y = collectionView.contentOffset.y + direction * velocity * CGFloat(dt)
        y = min(max(y, top), bottom)
        collectionView.contentOffset.y = y

        let timedOut = CACurrentMediaTime() - startedAt > 180
        if direction < 0 {
            if y <= top + 0.5 {
                if hasMore(), !timedOut {
                    phase.topStallFrames += 1
                    // Held against the top until the page lands, then on up.
                    velocity = max(velocity, 2500)
                } else {
                    finishPhase(next: "down", direction: 1)
                }
            } else if velocity < 1200 {
                fling(-1)
            }
        } else {
            if y >= bottom - 0.5 {
                finishPhase(next: nil, direction: 0)
            } else if velocity < 1200 {
                fling(1)
            }
        }
    }

    private func sampleTiles(_ collectionView: UICollectionView) {
        let visible = collectionView.bounds.inset(by: collectionView.adjustedContentInset)
        var blank = 0
        for cell in collectionView.visibleCells where cell.frame.intersects(visible) {
            guard let state = tileState(cell) else { continue }
            phase.tiles += 1
            if !state.image { blank += 1 } else if !state.preview { phase.placeholderTiles += 1 }
        }
        phase.blankTiles += blank
        if blank > 0 { phase.blankFrames += 1 }
    }

    private func finishPhase(next: String?, direction: CGFloat) {
        phases.append(phase)
        watches.append(watch.json)
        watch.reset()
        NSLog("STUDIO-BENCH phase %@ done", phase.name)
        guard let next else { return finish() }
        phase = Phase(name: next)
        fling(direction)
    }

    private func finish() {
        link?.invalidate()
        link = nil
        watch.stop()
        var result: [String: Any] = ["perf": StudioGalleryPerf.json]
        if let collectionView {
            result["items"] = collectionView.numberOfItems(inSection: 0)
        }
        for (phase, watch) in zip(phases, watches) {
            var json = phase.json
            json["mainThread"] = watch
            result[phase.name] = json
        }
        let data = (try? JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])) ?? Data()
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        try? data.write(to: docs.appendingPathComponent("studio-bench.json"))
        let text = String(decoding: data, as: UTF8.self)
        NSLog("STUDIO-BENCH %@", text)
        if let window = collectionView?.window {
            label.numberOfLines = 0
            label.font = .monospacedSystemFont(ofSize: 9, weight: .regular)
            label.backgroundColor = UIColor.systemBackground.withAlphaComponent(0.85)
            label.text = text
            label.accessibilityIdentifier = "studio-bench-result"
            label.frame = window.bounds.insetBy(dx: 12, dy: 80)
            window.addSubview(label)
        }
        retained = nil
    }
}
