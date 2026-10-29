import UIKit

/// Where the viewer's zoom transition finds the thumbnail it opens from and
/// closes back to: previews register their image views by artifact id, and
/// the gallery answers for cells that are scrolled away.
@MainActor
final class StudioGalleryTransitionSource {
    weak var provider: StudioGalleryTransitionSourceProvider?
    private var registeredViews: [String: WeakStudioTransitionView] = [:]

    func imageView(for artifactId: String) -> UIView? {
        if let view = registeredViews[artifactId]?.view,
           view.window != nil,
           !view.isHidden,
           view.alpha > 0.01 {
            return view
        }
        registeredViews[artifactId] = nil
        return provider?.transitionImageView(for: artifactId)
    }

    func register(_ view: UIView, for artifactId: String) {
        registeredViews[artifactId] = WeakStudioTransitionView(view)
    }

    func unregister(_ view: UIView, for artifactId: String) {
        guard registeredViews[artifactId]?.view === view else { return }
        registeredViews[artifactId] = nil
    }

    func prepareForDismissal(to artifactId: String) {
        provider?.prepareTransitionImageView(for: artifactId)
    }
}

private final class WeakStudioTransitionView {
    weak var view: UIView?
    init(_ view: UIView) { self.view = view }
}

@MainActor
protocol StudioGalleryTransitionSourceProvider: AnyObject {
    func transitionImageView(for artifactId: String) -> UIView?
    func prepareTransitionImageView(for artifactId: String)
}

extension StudioGalleryTransitionSourceProvider {
    func prepareTransitionImageView(for artifactId: String) {}
}

/// One artifact's preview: the cached preview or its thumbhash at once, the
/// relayed preview when it lands, a media glyph until then.
final class StudioMediaPreviewView: UIView {
    private let browser: StudioBrowserStore
    private let service: StudioService
    private let imageView = UIImageView()
    private let placeholder = UIImageView()
    private var artifactId: String?
    private weak var transitionSource: StudioGalleryTransitionSource?
    private var load: Task<Void, Never>?
    /// The preview this view holds demand on, until it lands or is let go:
    /// a row scrolled away mustn't keep its relay read alive.
    private var requestedId: String?
    /// The relayed preview is showing (not a thumbhash or the glyph).
    private var loaded = false

    init(browser: StudioBrowserStore, service: StudioService) {
        self.browser = browser
        self.service = service
        super.init(frame: .zero)
        backgroundColor = Palette.chip
        clipsToBounds = true
        imageView.clipsToBounds = true
        imageView.isUserInteractionEnabled = false
        placeholder.tintColor = Palette.color(.textFaint).withAlphaComponent(0.5)
        placeholder.contentMode = .center
        placeholder.preferredSymbolConfiguration = UIImage.SymbolConfiguration(pointSize: 20, weight: .light)
        for view in [placeholder, imageView] {
            view.frame = bounds
            view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            addSubview(view)
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(
        artifactId: String,
        mediaKind: StudioMediaKind,
        thumbhash: String? = nil,
        aspectRatio: CGFloat? = nil,
        contentMode: UIView.ContentMode = .scaleAspectFill,
        transitionSource: StudioGalleryTransitionSource? = nil
    ) {
        imageView.contentMode = contentMode
        if self.artifactId != artifactId || self.transitionSource !== transitionSource,
           let previous = self.artifactId {
            self.transitionSource?.unregister(imageView, for: previous)
        }
        // Every time: the source forgets a view it found off screen, and a
        // row scrolled back must be found again.
        transitionSource?.register(imageView, for: artifactId)
        self.transitionSource = transitionSource
        placeholder.image = UIImage(systemName: mediaKind == .video ? "film" : "photo")
        let same = self.artifactId == artifactId
        // The same artifact again: nothing to do unless its preview failed
        // (retry) or it had nothing to show yet (a thumbhash may have come).
        if same, loaded || requestedId != nil {
            if imageView.image == nil { show(browser.thumbhashImage(thumbhash, aspectRatio: aspectRatio)) }
            return
        }
        self.artifactId = artifactId
        if !same {
            endRequest()
            loaded = false
        }
        if let cached = browser.cachedPreview(artifactId: artifactId) {
            loaded = true
            return show(cached)
        }
        if !same || imageView.image == nil { show(browser.thumbhashImage(thumbhash, aspectRatio: aspectRatio)) }
        guard let deviceId = browser.selectedDeviceId else { return }
        // Not `browser.preview`: cancelling its caller leaves the read
        // running until it finishes on its own.
        let request = browser.beginPreviewRequest(artifactId: artifactId, deviceId: deviceId, workspace: service)
        requestedId = artifactId
        load = Task { [weak self] in
            let image = await request.value
            guard !Task.isCancelled, let self, self.artifactId == artifactId else { return }
            self.endRequest()
            guard let image else { return }
            self.loaded = true
            self.show(image)
        }
    }

    /// Drop the artifact (cell reuse, removal).
    func reset() {
        endRequest()
        if let artifactId { transitionSource?.unregister(imageView, for: artifactId) }
        artifactId = nil
        loaded = false
        show(nil)
    }

    private func endRequest() {
        load?.cancel()
        load = nil
        if let requestedId { browser.endPreviewRequest(artifactId: requestedId) }
        requestedId = nil
    }

    /// The view a viewer transition zooms from and back to.
    var transitionView: UIView { imageView }

    private func show(_ image: UIImage?) {
        imageView.image = image
        placeholder.isHidden = image != nil
    }
}

/// A centered loading state: the system spinner over a short caption.
final class StudioLoadingView: UIView {
    init(_ caption: String) {
        super.init(frame: .zero)
        let spinner = UIActivityIndicatorView(style: .medium)
        spinner.startAnimating()
        let label = UILabel()
        label.text = caption
        label.font = Fonts.ui(.sans, 12)
        label.textColor = Palette.color(.textFaint)
        let stack = UIStackView(arrangedSubviews: [spinner, label])
        stack.axis = .vertical
        stack.spacing = 14
        stack.alignment = .center
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.centerXAnchor.constraint(equalTo: centerXAnchor),
            stack.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }
}
