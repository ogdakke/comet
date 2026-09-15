/// <reference types="@cloudflare/vitest-pool-workers" />

declare module "cloudflare:test" {
  interface ProvidedEnv {
    TEST_LOG: DurableObjectNamespace;
    TEST_STUDIO: DurableObjectNamespace;
    PREVIEW_ROOMS: DurableObjectNamespace;
  }
}
