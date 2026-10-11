import { expect, it, vi } from "vitest";
import { iceServers, type Env } from "../src/worker";

it("requires explicit TURN opt-in even with secrets, extra servers and cached credentials", async () => {
  const env = {
    TURN_KEY_ID: "test-key",
    TURN_KEY_TOKEN: "test-token",
    EXTRA_ICE: JSON.stringify([{ urls: "turn:extra.test:3478" }]),
  } as Env;
  const stun = [{ urls: "stun:stun.cloudflare.com:3478" }];
  const mint = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(JSON.stringify({
    iceServers: [{ urls: "turn:cloudflare.test:3478" }],
  })));
  try {
    for (const TURN_ENABLED of [undefined, "false", "TRUE", "1"]) {
      expect(await iceServers({ ...env, TURN_ENABLED })).toEqual(stun);
    }
    expect(mint).not.toHaveBeenCalled();
    const enabled = await iceServers({ ...env, TURN_ENABLED: "true" });
    expect(enabled).toEqual([
      { urls: ["turn:cloudflare.test:3478"] }, { urls: "turn:extra.test:3478" },
    ]);
    expect(mint).toHaveBeenCalledTimes(1);
    expect(await iceServers(env)).toEqual(stun); // cached credentials stay disabled
    expect(mint).toHaveBeenCalledTimes(1);
  } finally {
    mint.mockRestore();
  }
});
