import { expect, it } from "vitest";
// @ts-expect-error plain JS served to the browser
import { auth, slug } from "../public/tag.js";

// Vectors printed by relay_core::{slug, tag} (core/src/lib.rs).
it("matches relay_core::slug and relay_core::tag", async () => {
  expect(slug(" Big Filthy_Papaya! ")).toBe("big-filthy-papaya");
  expect(slug("Ünïcode Rööm")).toBe("n-code-r-m");
  expect(await auth(" Big Filthy_Papaya! ", "hunter2")).toBe("a33597215e97f41b");
  expect(await auth("big-filthy-papaya", "")).toBe("50930b5eaf5e82fb");
  expect(await auth("Ünïcode Rööm", "pässwörd")).toBe("9d37a6b011c8e3b2");
});
