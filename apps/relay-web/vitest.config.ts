import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [cloudflareTest({
    wrangler: { configPath: "./wrangler.jsonc" },
    miniflare: { bindings: { TURN_ENABLED: "true", EXTRA_ICE: JSON.stringify([{ urls: "turn:relay.test:3478" }]) } },
  })],
});
