import { describe, expect, it } from "vitest";
import { pickLiveHost } from "./device-room";

// The bug this guards: a host whose uplink died silently leaves a socket the
// runtime still lists (no close event ever fires, and the supersede `close()`
// never completes either). Routing to the FIRST host socket pinned the room to
// that corpse — client frames vanished into it while the live host, which had
// reconnected and sat later in the list, received nothing. A non-empty host
// list also suppressed the `host_offline` bounce, so clients hung instead of
// failing fast.
describe("device-room host selection", () => {
  const NOW = 1_000_000_000_000;
  const fresh = NOW - 10_000;
  const older = NOW - 10 * 60_000;

  it("prefers the newer host over an older one listed first", () => {
    expect(
      pickLiveHost([
        { ws: "corpse", lastSeenAt: older },
        { ws: "live", lastSeenAt: fresh }
      ])
    ).toBe("live");
  });

  it("still finds the newer host when the older one is listed last", () => {
    expect(
      pickLiveHost([
        { ws: "live", lastSeenAt: fresh },
        { ws: "corpse", lastSeenAt: older }
      ])
    ).toBe("live");
  });

  it("picks the freshest of several hosts", () => {
    expect(
      pickLiveHost([
        { ws: "older", lastSeenAt: NOW - 40_000 },
        { ws: "newest", lastSeenAt: NOW - 1_000 },
        { ws: "middle", lastSeenAt: NOW - 20_000 }
      ])
    ).toBe("newest");
  });

  it("keeps an idle host whose lastSeenAt is far in the past", () => {
    // Protocol pings never stamp lastSeenAt. An idle VPS joined 10 minutes
    // ago is still the host; a silence window here is what broke the model
    // picker.
    expect(pickLiveHost([{ ws: "idle-vps", lastSeenAt: older }])).toBe("idle-vps");
  });

  it("treats a socket attached before this deploy (no timestamps) as dead", () => {
    expect(pickLiveHost([{ ws: "legacy", lastSeenAt: 0 }])).toBeUndefined();
  });

  it("keeps a just-joined host", () => {
    expect(pickLiveHost([{ ws: "joining", lastSeenAt: NOW }])).toBe("joining");
  });

  it("has no host in an empty room", () => {
    expect(pickLiveHost([])).toBeUndefined();
  });

  // The runtime still lists a socket while its close is being handled. Counting
  // it as live made `webSocketClose` skip the broadcast, so clients were never
  // told their host had left and sat on a link that would never answer again.
  it("skips the socket whose close is being handled", () => {
    expect(pickLiveHost([{ ws: "leaving", lastSeenAt: fresh }], "leaving")).toBeUndefined();
  });

  it("still finds a successor when the departing socket is excluded", () => {
    expect(
      pickLiveHost(
        [
          { ws: "leaving", lastSeenAt: NOW },
          { ws: "successor", lastSeenAt: fresh }
        ],
        "leaving"
      )
    ).toBe("successor");
  });
});
