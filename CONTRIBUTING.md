# Contributing

RELAY is open source under the [Mozilla Public License 2.0](LICENSE).

## Build the plugin

From `apps/plugin`:

```bash
cargo truce install
```

That installs every format declared in this crate's default features:

| Format | Feature | Where it lands |
|---|---|---|
| CLAP | `clap` | `~/.clap/RELAY.clap` |
| VST3 | `vst3` | `~/.vst3/RELAY.vst3` |
| VST2 | `vst2` | `~/.vst/RELAY.so` (Linux) / `~/Library/Audio/Plug-Ins/VST/` (macOS) |
| LV2 | `lv2` | `~/.lv2/relay.lv2` |
| AU v2 | `au` | `~/Library/Audio/Plug-Ins/Components/` (macOS only) |
| AU v3 | `au` | container app via `cargo truce install --au3` (macOS + Xcode) |
| Standalone | `standalone` | `apps/plugin/target/release/relay-plugin-standalone` |

AAX is opt-in (`--features aax` / `cargo truce install --aax`) and needs the Avid AAX SDK. VST2 uses Truce's clean-room shim — no Steinberg SDK.

Subset:

```bash
cargo truce install --clap --vst3 --lv2
cargo truce install --vst2
cargo truce install --au2          # macOS
cargo truce install --au3          # macOS, Xcode
```

Workspace checks live at the repo root (`just check`, `just test`).

## Cut a release

Releases are built on a developer machine, not in CI: `apps/plugin` compiles
against a sibling Truce checkout that CI does not have.

```bash
./scripts/release.sh                      # gate, then package into dist/
./scripts/release.sh build --no-notarize  # package only, no Apple credentials
./scripts/release.sh publish v0.1.0       # upload dist/ to a GitHub release
```

A host builds its own platform: the Linux run makes a tarball plus
`install.sh`, macOS makes a `.pkg`, Windows makes a `.exe`. Run it on each OS
you ship, copy the results into one `dist/`, then publish from there — the
checksum file is regenerated over everything staged.

## License

By contributing you agree that your changes are licensed under MPL-2.0, same as the rest of the tree. Keep the SPDX identifier `MPL-2.0` on new crates and `package.json` files.
