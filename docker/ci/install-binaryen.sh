#!/usr/bin/env bash
# Installs binaryen (wasm-opt, which trunk runs on the interface's WebAssembly) at one pinned
# version on every platform: the Docker image, CI and the release builds. Distributions ship
# versions years apart, and an old wasm-opt can turn the output of a newer Rust into WebAssembly
# that does not run. Each download is checked against the checksum the binaryen release
# publishes for it.
#
#   docker/ci/install-binaryen.sh DIR      installs to DIR, with wasm-opt in DIR/bin
set -euo pipefail

version=133
dir="${1:?usage: install-binaryen.sh DIR}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    platform=x86_64-linux
    sha=2dc9c7813f5375db93d96ead4b78222fcc3e2677bbb832297af4797782a37489 ;;
  Linux-aarch64 | Linux-arm64)
    platform=aarch64-linux
    sha=89c07ea56faf38d0fbecf36ca8ec0721756716185f265b568e133d427f299bf8 ;;
  Darwin-arm64)
    platform=arm64-macos
    sha=ad66da82ac13f163e424b1643f16c6dfcccc98b5966296b43e52d3cab04f84a8 ;;
  Darwin-x86_64)
    platform=x86_64-macos
    sha=13a9b90be775c6389ce3d1f879cb8627bea56708ba8c122983941d53a8199b95 ;;
  MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64)
    platform=x86_64-windows
    sha=17a2cbeac6b5693c5fbafab3838d3c65fd9c1eb38b05f5baec6c657e8c84995b ;;
  *)
    echo "binaryen publishes no build for $(uname -s) $(uname -m)" >&2
    exit 1 ;;
esac

file="binaryen-version_${version}-${platform}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$file" \
  "https://github.com/WebAssembly/binaryen/releases/download/version_${version}/${file}"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$tmp" && echo "${sha}  ${file}" | sha256sum -c -)
else
  (cd "$tmp" && echo "${sha}  ${file}" | shasum -a 256 -c -)
fi
mkdir -p "$dir"
tar -xzf "$tmp/$file" -C "$dir" --strip-components=1
"$dir/bin/wasm-opt" --version
