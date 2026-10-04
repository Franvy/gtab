#!/usr/bin/env bash
set -euo pipefail

# Packages the release tarball and the Homebrew bottle for the current Cargo
# version, then renders the tap formula that points at both.
#
# The bottle lets Homebrew pour gtab instead of treating the install as a
# source build, which would require up-to-date Command Line Tools.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

version="$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$version" ]]; then
  echo "failed to read package version from Cargo.toml" >&2
  exit 1
fi

output="${1:-.release/homebrew-gtab/Formula/gtab.rb}"
release_url="https://github.com/Franvy/gtab/releases/download/v${version}"
release_tar="dist/gtab-${version}-aarch64-apple-darwin.tar.gz"
# Homebrew downloads `<root_url>/<name>-<version>.<tag>.bottle.tar.gz`.
bottle_tar="dist/gtab-${version}.all.bottle.tar.gz"

if ! grep -q "tag: \"v${version}\"" Formula/gtab.rb; then
  echo "Formula/gtab.rb is not at v${version}; run scripts/render-homebrew-formula.sh first" >&2
  exit 1
fi

mkdir -p dist

# Reuse archives that already exist: their checksums may already be published.
if [[ -f "$release_tar" ]]; then
  echo "Reusing ${release_tar}"
else
  binary_version="$(target/release/gtab --version 2>/dev/null || true)"
  if [[ "$binary_version" != "gtab ${version}" ]]; then
    echo "target/release/gtab is not v${version}; run cargo build --release first" >&2
    exit 1
  fi
  COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -czf "$release_tar" -C target/release gtab
  echo "Wrote ${release_tar}"
fi

if [[ -f "$bottle_tar" ]]; then
  echo "Reusing ${bottle_tar}"
else
  # Build the bottle from the release tarball so both assets ship the same binary.
  stage="$(mktemp -d "${TMPDIR:-/tmp}/gtab-bottle.XXXXXX")"
  trap 'rm -rf "$stage"' EXIT
  mkdir -p "$stage/gtab/${version}/bin"
  tar -xzf "$release_tar" -C "$stage/gtab/${version}/bin" gtab
  COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -czf "$bottle_tar" -C "$stage" gtab
  echo "Wrote ${bottle_tar}"
fi

release_sha="$(shasum -a 256 "$release_tar" | cut -d' ' -f1)"
bottle_sha="$(shasum -a 256 "$bottle_tar" | cut -d' ' -f1)"

mkdir -p "$(dirname "$output")"

{
  cat <<EOF
class Gtab < Formula
  desc "Ghostty tab workspace manager with an interactive TUI"
  homepage "https://github.com/Franvy/gtab"
  url "${release_url}/gtab-${version}-aarch64-apple-darwin.tar.gz"
  version "${version}"
  sha256 "${release_sha}"
  license "MIT"

  bottle do
    root_url "${release_url}"
    sha256 cellar: :any_skip_relocation, all: "${bottle_sha}"
  end

  depends_on arch: :arm64
  depends_on :macos

  def install
    bin.install "gtab"
  end

EOF
  # Caveats and tests stay in sync with the source-build formula.
  sed -n '/^  def caveats/,$p' Formula/gtab.rb
} > "$output"

echo "Wrote ${output} for v${version}"
echo "Upload both archives to the v${version} GitHub Release:"
echo "  ${release_tar}"
echo "  ${bottle_tar}"
