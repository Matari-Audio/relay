#!/usr/bin/env python3
"""Publish a RELAY release to the matari-audio.com R2 manifest.

Adapted from KURV's scripts/ci/publish-release.py: uploads the three platform
zips to R2 with wrangler and merges the release into manifest.json, which
drives the download panel. Notes come from the version's CHANGELOG.md section.

Needs CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID unless --dry-run.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from datetime import datetime, timezone

PLUGIN_ID = "relay"
PRODUCT_NAME = "RELAY"
CHANNEL = "stable"
BUCKET = os.environ.get("R2_BUCKET", "dolmen-gate-plugin-releases")
RETENTION = int(os.environ.get("RELEASE_RETENTION", "8"))
ROOT = pathlib.Path(__file__).resolve().parents[1]
LABELS = {"linux": "Linux", "windows": "Windows", "macos": "macOS"}
TRUST = {  # (signature_status, notarization_status), as the site expects
    "macos": ("verified", "notarized"),
    "windows": ("unsigned", "not_applicable"),
    "linux": ("not_applicable", "not_applicable"),
}


def wrangler(*args: str) -> None:
    subprocess.run(["npx", "-y", "wrangler", *args], check=True)


def version() -> str:
    return tomllib.loads((ROOT / "plugin" / "Cargo.toml").read_text())["package"]["version"]


def release_notes(ver: str) -> list[str]:
    lines = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8").splitlines()
    heading = re.compile(r"^##\s+\[?" + re.escape(ver) + r"\]?(\s.*)?$")
    start = next((i + 1 for i, l in enumerate(lines) if heading.match(l.strip())), None)
    if start is None:
        raise SystemExit(f"Missing CHANGELOG.md entry for version {ver}")
    notes = []
    for line in lines[start:]:
        s = line.strip()
        if s.startswith("## "):
            break
        if s.startswith(("-", "*")) and s[1:].strip():
            notes.append(s[1:].strip())
    if not notes:
        raise SystemExit(f"CHANGELOG.md entry for version {ver} has no bullet notes")
    return notes


def merge_manifest(manifest: dict, release: dict) -> tuple[dict, list[str]]:
    # Same merge as KURV publish-release.py / release-plugins-podman.sh.
    products = manifest.setdefault("products", [])
    old = next((p for p in products if p.get("id") == PLUGIN_ID), None)
    old_releases = (old or {}).get("releases", []) or []
    payloads = [release] + [r for r in old_releases if r.get("version") != release["version"]]
    payloads = payloads[:RETENTION]
    kept = {r.get("version") for r in payloads}
    stale = [a["key"] for r in old_releases if r.get("version") not in kept
             for a in r.get("assets", []) or [] if a.get("key")]
    channels = (old or {}).get("channels", {}) or {}
    channels[CHANNEL] = {"latest_version": release["version"]}
    product = old or {"id": PLUGIN_ID}
    product.update(name=PRODUCT_NAME, latest_version=release["version"], status=CHANNEL,
                   channels=channels, releases=payloads)
    if old is None:
        products.append(product)
    return manifest, stale


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux", type=pathlib.Path, required=True)
    parser.add_argument("--windows", type=pathlib.Path, required=True)
    parser.add_argument("--macos-pkg", type=pathlib.Path, required=True)
    parser.add_argument("--provenance-url", required=True)
    parser.add_argument("--dry-run", action="store_true", help="print the payload, upload nothing")
    args = parser.parse_args()
    for name in ("CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID"):
        if not args.dry_run and not os.environ.get(name):
            raise SystemExit(f"{name} must be set")

    ver = version()
    work = pathlib.Path(tempfile.mkdtemp())
    archives = {p: work / f"{PLUGIN_ID}-{p}-{ver}.zip" for p in LABELS}
    archives["linux"].write_bytes(args.linux.read_bytes())
    archives["windows"].write_bytes(args.windows.read_bytes())
    with zipfile.ZipFile(archives["macos"], "w", zipfile.ZIP_DEFLATED) as zf:
        zf.write(args.macos_pkg, args.macos_pkg.name)

    keys = {p: f"{PLUGIN_ID}/{ver}/{p}/{a.name}" for p, a in archives.items()}
    release = {
        "id": PLUGIN_ID,
        "name": PRODUCT_NAME,
        "version": ver,
        "date": datetime.now(timezone.utc).date().isoformat(),
        "channel": CHANNEL,
        "notes": "\n".join(release_notes(ver)),
        "assets": [{
            "platform": p,
            "label": LABELS[p],
            "key": keys[p],
            "size": str(a.stat().st_size),  # the site expects a string
            "sha256": hashlib.sha256(a.read_bytes()).hexdigest(),
            "signature_status": TRUST[p][0],
            "notarization_status": TRUST[p][1],
            "provenance_url": args.provenance_url,
            "formats": ["CLAP", "VST3", "AU"] if p == "macos" else ["CLAP", "VST3"],
        } for p, a in archives.items()],
    }
    print(json.dumps(release, indent=2))
    if args.dry_run:
        return 0

    for p, a in archives.items():
        wrangler("r2", "object", "put", f"{BUCKET}/{keys[p]}", "--remote", "--file", str(a),
                 "--content-type", "application/zip")
    old_path = work / "manifest.json"
    # Unlike KURV's copy, a failed read aborts: falling back to an empty
    # manifest would erase every other product from the download panel.
    wrangler("r2", "object", "get", f"{BUCKET}/manifest.json", "--remote", "--file", str(old_path))
    manifest = json.loads(old_path.read_text())
    manifest, stale = merge_manifest(manifest, release)
    new_path = work / "manifest.new.json"
    new_path.write_text(json.dumps(manifest, indent=2) + "\n")
    wrangler("r2", "object", "put", f"{BUCKET}/manifest.json", "--remote", "--file", str(new_path),
             "--content-type", "application/json")
    for key in stale:
        subprocess.run(["npx", "-y", "wrangler", "r2", "object", "delete", f"{BUCKET}/{key}", "--remote"])
    print(f"Published {PRODUCT_NAME} {ver} to {BUCKET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
