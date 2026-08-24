// The two top-level areas of the phone app. Each tab owns its navigation
// stack, so switching between Chat and Studio preserves both histories.

import SwiftUI

private enum AppSection: Hashable {
    case chat
    case studio
}

struct AppShellView: View {
    @State private var section: AppSection = .chat

    var body: some View {
        TabView(selection: $section) {
            Tab("Chat", systemImage: "bubble.left.and.bubble.right", value: .chat) {
                HomeView()
            }

            Tab("Studio", systemImage: "photo.stack", value: .studio) {
                StudioRootView()
            }
        }
        // iOS does not minimize tab bars under the automatic policy. Opt in
        // to the compact leading icon while someone scrolls through content.
        .tabBarMinimizeBehavior(.onScrollDown)
    }
}
