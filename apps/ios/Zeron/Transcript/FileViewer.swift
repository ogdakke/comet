import AVKit
import QuickLook
import UIKit
import UniformTypeIdentifiers
import WebKit

/// A file an agent sent (a `file` widget), with where to fetch it from.
struct SharedFile {
    let name: String
    let mimeType: String
    let path: String
    let transferId: String?
    let size: UInt64

    /// Plays or shows as a picture: opens in the thread's gallery rather
    /// than the file viewer (SVG renders as a document there).
    var isGalleryMedia: Bool {
        mimeType.hasPrefix("video/") || (mimeType.hasPrefix("image/") && mimeType != "image/svg+xml")
    }
}

/// Opens a shared file: streams audio/video, renders HTML/SVG in a sandboxed
/// web view and Markdown natively, and hands everything else to QuickLook.
/// Presented in its own navigation controller (Done, Share).
final class FileViewerController: UIViewController {
    private let file: SharedFile
    private let client: CoreClient
    private let chatId: String
    private let hostDevice: String
    private var localURL: URL?
    private var loadTask: Task<Void, Never>?
    private var content: UIViewController?

    private let progress = UIProgressView(progressViewStyle: .bar)
    private let status = UILabel()
    private let retry = UIButton(type: .system)
    private let stack = UIStackView()
    private lazy var shareItem = UIBarButtonItem(systemItem: .action, primaryAction: UIAction { [weak self] _ in self?.share() })

    init(file: SharedFile, client: CoreClient, chatId: String, hostDevice: String) {
        self.file = file
        self.client = client
        self.chatId = chatId
        self.hostDevice = hostDevice
        super.init(nibName: nil, bundle: nil)
        title = file.name
        modalPresentationStyle = .fullScreen
    }

    required init?(coder: NSCoder) { fatalError() }

    deinit { loadTask?.cancel() }

    /// The viewer inside its navigation controller, ready to present.
    static func presentable(file: SharedFile, client: CoreClient, chatId: String, hostDevice: String) -> UINavigationController {
        let nav = UINavigationController(rootViewController: FileViewerController(file: file, client: client, chatId: chatId, hostDevice: hostDevice))
        nav.modalPresentationStyle = .fullScreen
        return nav
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        navigationItem.leftBarButtonItem = UIBarButtonItem(systemItem: .done, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        shareItem.isEnabled = false
        navigationItem.rightBarButtonItem = shareItem

        status.font = Fonts.ui(.sans, 15)
        status.textColor = Palette.secondary
        status.textAlignment = .center
        status.numberOfLines = 0
        progress.progressTintColor = Palette.accent
        var config = UIButton.Configuration.tinted()
        config.title = "Retry"
        config.cornerStyle = .capsule
        config.baseForegroundColor = Palette.accent
        retry.configuration = config
        retry.addAction(UIAction { [weak self] _ in self?.load() }, for: .touchUpInside)
        stack.axis = .vertical
        stack.alignment = .center
        stack.spacing = 14
        for v in [progress, status, retry] { stack.addArrangedSubview(v) }
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            stack.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 32),
            stack.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -32),
            progress.widthAnchor.constraint(equalTo: stack.widthAnchor),
        ])
        load()
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isBeingDismissed || navigationController?.isBeingDismissed == true { loadTask?.cancel() }
    }

    private var request: SharedFileRequest {
        SharedFileRequest(deviceId: hostDevice, chatId: chatId, transferId: file.transferId, path: file.path, name: file.name)
    }

    private var isStreamable: Bool { file.mimeType.hasPrefix("video/") || file.mimeType.hasPrefix("audio/") }

    private func load() {
        loadTask?.cancel()
        retry.isHidden = true
        progress.isHidden = false
        progress.progress = 0
        status.text = "Downloading…"
        loadTask = Task { @MainActor [weak self] in
            guard let self else { return }
            if self.isStreamable, let transferId = self.file.transferId,
               let remote = try? await self.client.sharedFileUrl(chatId: self.chatId, transferId: transferId),
               let url = URL(string: remote)
            {
                self.show(player: url)
                return
            }
            let poll = Task { @MainActor [weak self] in
                while !Task.isCancelled {
                    if let self, let p = self.client.sharedFileProgress(file: self.request) { self.progress.setProgress(Float(p), animated: true) }
                    try? await Task.sleep(for: .milliseconds(300))
                }
            }
            defer { poll.cancel() }
            do {
                let path = try await self.client.fetchSharedFile(file: self.request)
                guard !Task.isCancelled else { return }
                self.show(local: URL(fileURLWithPath: path))
            } catch {
                guard !Task.isCancelled else { return }
                self.progress.isHidden = true
                self.status.text = "Couldn't open \(self.file.name): \(error)"
                self.retry.isHidden = false
            }
        }
    }

    // MARK: Showing

    private func present(content vc: UIViewController) {
        content?.willMove(toParent: nil)
        content?.view.removeFromSuperview()
        content?.removeFromParent()
        addChild(vc)
        vc.view.frame = view.bounds
        vc.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(vc.view)
        vc.didMove(toParent: self)
        content = vc
        stack.isHidden = true
    }

    private func show(player url: URL) {
        let vc = AVPlayerViewController()
        vc.player = AVPlayer(url: url)
        present(content: vc)
        vc.player?.play()
    }

    private func show(local url: URL) {
        localURL = url
        shareItem.isEnabled = true
        let mime = file.mimeType
        if mime.hasPrefix("video/") || mime.hasPrefix("audio/") {
            show(player: url)
        } else if mime == "text/html" || mime == "image/svg+xml" {
            present(content: WebFileController(url: url, mime: mime))
        } else if mime == "text/markdown" {
            present(content: MarkdownFileController(url: url))
        } else {
            let ql = QLPreviewController()
            ql.dataSource = self
            present(content: ql)
        }
    }

    private func share() {
        guard let localURL else { return }
        let sheet = UIActivityViewController(activityItems: [localURL], applicationActivities: nil)
        sheet.popoverPresentationController?.barButtonItem = shareItem
        present(sheet, animated: true)
    }
}

extension FileViewerController: QLPreviewControllerDataSource {
    func numberOfPreviewItems(in controller: QLPreviewController) -> Int { localURL == nil ? 0 : 1 }

    func previewController(_ controller: QLPreviewController, previewItemAt index: Int) -> QLPreviewItem {
        (localURL ?? URL(fileURLWithPath: "/")) as NSURL
    }
}

/// Rendered / source toggle shared by the HTML and Markdown viewers.
private func makeModeControl(_ target: UIViewController, action: @escaping (Int) -> Void) -> UISegmentedControl {
    let control = UISegmentedControl(items: ["Rendered", "Source"])
    control.selectedSegmentIndex = 0
    control.addAction(UIAction { [unowned control] _ in action(control.selectedSegmentIndex) }, for: .valueChanged)
    control.translatesAutoresizingMaskIntoConstraints = false
    target.view.addSubview(control)
    NSLayoutConstraint.activate([
        control.topAnchor.constraint(equalTo: target.view.safeAreaLayoutGuide.topAnchor, constant: 8),
        control.centerXAnchor.constraint(equalTo: target.view.centerXAnchor),
        control.widthAnchor.constraint(equalToConstant: 220),
    ])
    return control
}

private func makeSourceView(_ text: String) -> UITextView {
    let tv = UITextView()
    tv.text = text
    tv.isEditable = false
    tv.font = Fonts.ui(.mono, 13)
    tv.textColor = Palette.text
    tv.backgroundColor = Palette.background
    tv.textContainerInset = UIEdgeInsets(top: 16, left: 14, bottom: 32, right: 14)
    return tv
}

/// HTML or SVG an agent wrote, in a web view with no persistent storage that
/// can't leave its own file: scripts may run, but nothing outside the file's
/// folder is readable and every navigation away is cancelled.
final class WebFileController: UIViewController, WKNavigationDelegate {
    private let url: URL
    private let mime: String
    private let webView: WKWebView
    private let source: UITextView

    init(url: URL, mime: String) {
        self.url = url
        self.mime = mime
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        webView = WKWebView(frame: .zero, configuration: config)
        source = makeSourceView((try? String(contentsOf: url, encoding: .utf8)) ?? "(Not text)")
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        webView.navigationDelegate = self
        webView.isOpaque = false
        source.isHidden = true
        for v in [webView, source] {
            v.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(v)
        }
        let mode = makeModeControl(self) { [weak self] i in
            self?.webView.isHidden = i == 1
            self?.source.isHidden = i == 0
        }
        for v in [webView, source] {
            NSLayoutConstraint.activate([
                v.topAnchor.constraint(equalTo: mode.bottomAnchor, constant: 8),
                v.leadingAnchor.constraint(equalTo: view.leadingAnchor),
                v.trailingAnchor.constraint(equalTo: view.trailingAnchor),
                v.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            ])
        }
        webView.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
    }

    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
        guard let target = action.request.url else { return decisionHandler(.cancel) }
        // The file itself (and the frames it renders into) loads; anything
        // else is a link out.
        if target == url || target.absoluteString == "about:blank" || (target.isFileURL && target.path == url.path) {
            return decisionHandler(.allow)
        }
        decisionHandler(.cancel)
        guard action.navigationType == .linkActivated, target.scheme == "http" || target.scheme == "https" else { return }
        let alert = UIAlertController(title: "Open in Safari?", message: target.absoluteString, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Open", style: .default) { _ in UIApplication.shared.open(target) })
        present(alert, animated: true)
    }
}

/// A Markdown file rendered natively, with a source toggle.
final class MarkdownFileController: UIViewController {
    private let rendered = UITextView()
    private let source: UITextView

    init(url: URL) {
        let text = (try? String(contentsOf: url, encoding: .utf8)) ?? "(Not text)"
        source = makeSourceView(text)
        super.init(nibName: nil, bundle: nil)
        rendered.isEditable = false
        rendered.backgroundColor = Palette.background
        rendered.textContainerInset = UIEdgeInsets(top: 16, left: 14, bottom: 32, right: 14)
        rendered.attributedText = Self.render(text)
    }

    required init?(coder: NSCoder) { fatalError() }

    /// Block structure (headings, lists) survives as styled lines; inline
    /// styling and links come from `AttributedString(markdown:)`.
    private static func render(_ text: String) -> NSAttributedString {
        let base = Fonts.ui(.sans, UIFontMetrics(forTextStyle: .body).scaledValue(for: 16.5))
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .full)
        guard let parsed = try? AttributedString(markdown: text, options: options) else {
            return NSAttributedString(string: text, attributes: [.font: base, .foregroundColor: Palette.text])
        }
        let out = NSMutableAttributedString()
        var lastBlock: Int?
        for run in parsed.runs {
            var piece = String(parsed[run.range].characters)
            let intent = run.presentationIntent
            let block = intent?.components.first?.identity
            if let block, let lastBlock, block != lastBlock { out.append(NSAttributedString(string: "\n\n")) }
            lastBlock = block ?? lastBlock
            var font = base
            if let kind = intent?.components.first?.kind {
                switch kind {
                case let .header(level): font = Fonts.ui(.sansSemibold, base.pointSize * (level == 1 ? 1.5 : level == 2 ? 1.3 : 1.15))
                case .codeBlock: font = Fonts.ui(.mono, 13.5)
                case .listItem: piece = "• " + piece
                default: break
                }
            }
            if run.inlinePresentationIntent?.contains(.code) == true { font = Fonts.ui(.mono, 14) }
            var attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: Palette.text]
            if run.inlinePresentationIntent?.contains(.emphasized) == true || run.inlinePresentationIntent?.contains(.stronglyEmphasized) == true {
                var traits: UIFontDescriptor.SymbolicTraits = []
                if run.inlinePresentationIntent?.contains(.emphasized) == true { traits.insert(.traitItalic) }
                if run.inlinePresentationIntent?.contains(.stronglyEmphasized) == true { traits.insert(.traitBold) }
                if let d = font.fontDescriptor.withSymbolicTraits(traits) { attrs[.font] = UIFont(descriptor: d, size: font.pointSize) }
            }
            if let link = run.link {
                attrs[.link] = link
                attrs[.foregroundColor] = Palette.accent
            }
            out.append(NSAttributedString(string: piece, attributes: attrs))
        }
        return out
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = Palette.background
        source.isHidden = true
        for v in [rendered, source] {
            v.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(v)
        }
        let mode = makeModeControl(self) { [weak self] i in
            self?.rendered.isHidden = i == 1
            self?.source.isHidden = i == 0
        }
        for v in [rendered, source] {
            NSLayoutConstraint.activate([
                v.topAnchor.constraint(equalTo: mode.bottomAnchor, constant: 8),
                v.leadingAnchor.constraint(equalTo: view.leadingAnchor),
                v.trailingAnchor.constraint(equalTo: view.trailingAnchor),
                v.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            ])
        }
    }
}
