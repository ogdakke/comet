import { describe, expect, it } from "vitest";
import { handleAuthRoute } from "./auth-routes";
import type { Env } from "./env";

const secret = (value: string): SecretsStoreSecret =>
  ({ get: async () => value });

const envWithClient = {
  WORKOS_CLIENT_ID: secret("client_self_hosted")
} as Env;

describe("iOS self-hosted auth routes", () => {
  it("starts OAuth with the Worker's client and HTTPS callback", async () => {
    const url = new URL("https://edge.example/auth/ios/authorize?state=state-123");
    const response = await handleAuthRoute(new Request(url), envWithClient, url);

    expect(response?.status).toBe(302);
    expect(response?.headers.get("cache-control")).toBe("no-store");
    const location = new URL(response!.headers.get("location")!);
    expect(location.origin).toBe("https://api.workos.com");
    expect(location.pathname).toBe("/user_management/authorize");
    expect(location.searchParams.get("client_id")).toBe("client_self_hosted");
    expect(location.searchParams.get("redirect_uri")).toBe(
      "https://edge.example/auth/ios/callback"
    );
    expect(location.searchParams.get("state")).toBe("state-123");
  });

  it("rejects invalid state instead of starting OAuth", async () => {
    const url = new URL("https://edge.example/auth/ios/authorize?state=bad%20state");
    const response = await handleAuthRoute(new Request(url), envWithClient, url);

    expect(response?.status).toBe(400);
    await expect(response?.json()).resolves.toEqual({ error: "invalid state" });
  });

  it("relays successful and failed callbacks to the fixed app scheme", async () => {
    const successURL = new URL(
      "https://edge.example/auth/ios/callback?code=code-1&state=state-1"
    );
    const success = await handleAuthRoute(new Request(successURL), {} as Env, successURL);
    expect(success?.headers.get("location")).toBe("zeron://callback?state=state-1&code=code-1");

    const failureURL = new URL(
      "https://edge.example/auth/ios/callback?error=access_denied&error_description=Cancelled&state=state-1"
    );
    const failure = await handleAuthRoute(new Request(failureURL), {} as Env, failureURL);
    expect(failure?.headers.get("location")).toBe(
      "zeron://callback?state=state-1&error=access_denied&error_description=Cancelled"
    );
  });
});
