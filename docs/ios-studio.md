# iOS Studio

The first iPhone Studio slice is a read-only, low-storage viewport onto the
Studio engine running on an online desktop. It does not run providers or keep
the desktop artifact library on the phone.

## Information architecture

- **Chat** and **Studio** are top-level destinations in the system tab bar.
  The tab bar minimizes while scrolling and preserves an independent
  navigation stack for each destination.
- Studio opens with a compact Liquid Glass switcher for **Gallery** and
  **Threads** in the navigation bar. These are related views of the same
  library, so a segmented control is more appropriate than another tab bar.
- Gallery opens an artifact detail view. Threads open a chronological feed;
  each output in the feed opens the same artifact detail view.
- The Studio host is selected automatically, preferring the remembered online
  desktop and then another online desktop. Device choice is not exposed as
  primary Studio UI.

This follows Apple's guidance to use tab bars for top-level app sections and
segmented controls for closely related subviews. Standard navigation and
scroll containers supply the system's soft scroll-edge blur beneath the
floating control.

## View model

The phone decodes a narrow projection of the provider-neutral engine model:

```text
StudioThreadSummary
StudioThread
  StudioTurn
    StudioRun
      StudioArtifact

StudioGalleryItem
```

Thread summaries drive the library list. A watched thread supplies its ordered
turns, model runs, status, progress, errors, and artifacts. Gallery items carry
only the metadata needed for tiles and artifact details.

## Large-library behavior

Gallery RPCs support a stable `(createdAt, artifactId)` cursor. iOS watches the
newest 60 items and requests another bounded page when a visible tile enters
the final 12 items. The engine caps requested pages at 200. Existing desktop
clients that send an empty request retain the full-gallery response.

`LazyVGrid` creates only nearby tiles. Every tile owns an explicit square
layout container, so loading placeholders and media use identical geometry.
Updates to the first page preserve already loaded older pages when their anchor
still exists.

## Phone storage budget

- Gallery metadata is memory-only.
- Previews are fetched on demand from the owning desktop through the relay.
- Preview reads are capped at 8 MiB each and downsampled before display.
- Decoded previews use an `NSCache` capped at 48 entries and 24 MiB.
- No artifact or preview disk cache is created by Studio.
- Artifact detail shows the optimized preview. Video playback is explicitly
  unavailable rather than downloading an entire video to temporary storage.

## Initial scope

Included:

- paginated image/video gallery
- active and archived thread library
- live thread status and chronological turn feed
- read-only artifact details and metadata
- automatic online desktop failover

Deferred:

- generation composer
- editing
- upscaling
- using an artifact as a reference
- full-resolution downloads, sharing, and video playback

## Validation environments

Debug uses `apps/ios/Staging.xcconfig` and the staging identity associated with
`~/.zeron-dev`. Release uses `apps/ios/Production.xcconfig`. A revoked WorkOS
session must fail visibly and be renewed with a normal staging login; tests and
demo data do not masquerade as a live staging library.

References: [Apple Human Interface Guidelines: Tab bars](https://developer.apple.com/design/human-interface-guidelines/tab-bars),
[Segmented controls](https://developer.apple.com/design/human-interface-guidelines/segmented-controls), and
[ScrollEdgeEffectStyle](https://developer.apple.com/documentation/swiftui/scrolledgeeffectstyle).
