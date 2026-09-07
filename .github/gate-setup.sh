#!/usr/bin/env bash
# Native dependencies for RELAY's Ubuntu builds: libopus and libdatachannel.
# Shared by .github/workflows/ci-rust.yml and the Matari-Audio/ci-control gate.
set -euo pipefail

opus_version=1.6.1
opus_sha256=6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1
# Peeled commit of libdatachannel v0.24.5; see docs/research/probe-libdatachannel-build.md.
datachannel_tag=v0.24.5
datachannel_commit=443f6934d9007eb7076ab7825ba330f355fcbead

tmp="${RUNNER_TEMP:-$(mktemp -d)}"
archive="$tmp/opus-$opus_version.tar.gz"

sudo apt-get update
sudo apt-get install --yes ninja-build pkg-config libssl-dev

curl --fail --location --retry 3 \
  "https://downloads.xiph.org/releases/opus/opus-$opus_version.tar.gz" \
  --output "$archive"
echo "$opus_sha256  $archive" | sha256sum --check --strict
tar --extract --gzip --file "$archive" --directory "$tmp"
cmake \
  -S "$tmp/opus-$opus_version" \
  -B "$tmp/opus-build" \
  -G Ninja \
  -DBUILD_SHARED_LIBS=ON \
  -DOPUS_BUILD_PROGRAMS=OFF \
  -DOPUS_BUILD_TESTING=OFF \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_LIBDIR=lib \
  -DCMAKE_INSTALL_PREFIX=/usr/local
cmake --build "$tmp/opus-build" --parallel
sudo cmake --install "$tmp/opus-build"

# relay-libdatachannel-sys links -ldatachannel and needs the media symbols
# (rtcAddTrackEx, rtcSetOpusPacketizer), so NO_MEDIA stays off.
git clone --depth 1 --branch "$datachannel_tag" \
  --recurse-submodules --shallow-submodules \
  https://github.com/paullouisageneau/libdatachannel.git "$tmp/libdatachannel"
test "$(git -C "$tmp/libdatachannel" rev-parse HEAD)" = "$datachannel_commit"
cmake \
  -S "$tmp/libdatachannel" \
  -B "$tmp/libdatachannel-build" \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_SHARED_LIBS=ON \
  -DUSE_NICE=OFF \
  -DNO_WEBSOCKET=ON \
  -DNO_EXAMPLES=ON \
  -DNO_TESTS=ON \
  -DCMAKE_INSTALL_LIBDIR=lib \
  -DCMAKE_INSTALL_PREFIX=/usr/local
cmake --build "$tmp/libdatachannel-build" --target datachannel --parallel
sudo cmake --install "$tmp/libdatachannel-build"

sudo ldconfig

{
  echo "LIBRARY_PATH=/usr/local/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
  echo "LD_LIBRARY_PATH=/usr/local/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  echo "PKG_CONFIG_PATH=/usr/local/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
} >> "${GITHUB_ENV:-/dev/null}"
