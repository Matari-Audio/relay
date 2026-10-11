// Mirrors relay_core::slug and relay_core::tag (core/src/lib.rs). Keep them in step.
export function slug(room) {
  let out = "";
  for (const c of room.trim().toLowerCase()) {
    if (/^[a-z0-9]$/.test(c)) out += c;
    else if (out && !out.endsWith("-")) out += "-";
  }
  return out.slice(0, 48).replace(/-+$/, "");
}

// hex(first 8 bytes of SHA-256(slug(room) + "\0" + password))
export async function auth(room, password) {
  const bytes = new TextEncoder().encode(`${slug(room)}\0${password}`);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return [...digest.slice(0, 8)].map((b) => b.toString(16).padStart(2, "0")).join("");
}
