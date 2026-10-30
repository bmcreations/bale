#!/bin/sh
# Installs a released bale binary on macOS or Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/bmcreations/bale/main/install.sh | sh
#
# BALE_VERSION picks a release (default: the latest), BALE_INSTALL_DIR the
# directory (default: ~/.local/bin). Then run `bale install` for the sweep
# and `bale install --hooks claude` (or another tool) for the hooks.
set -eu

repo="bmcreations/bale"
dir="${BALE_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) echo "bale: no prebuilt binary for $(uname -s); build from source with cargo" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "bale: no prebuilt binary for $(uname -m)" >&2; exit 1 ;;
esac

version="${BALE_VERSION:-}"
if [ -z "$version" ]; then
  version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
  version="${version##*/v}"
fi
version="${version#v}"

archive="bale-v$version-$arch-$os.tar.gz"
url="https://github.com/$repo/releases/download/v$version/$archive"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "bale: downloading $archive"
curl -fsSL "$url" -o "$tmp/$archive"
curl -fsSL "$url.sha256" -o "$tmp/$archive.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$tmp" && shasum -a 256 -c "$archive.sha256" >/dev/null)
else
  (cd "$tmp" && sha256sum -c "$archive.sha256" >/dev/null)
fi

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$dir"
# Copy next to the target, then rename over it: a running bale keeps its
# old file instead of reading a half-written one.
cp "$tmp/bale" "$dir/.bale.new"
chmod +x "$dir/.bale.new"
mv -f "$dir/.bale.new" "$dir/bale"
echo "bale: installed $("$dir/bale" --version) to $dir/bale"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "bale: $dir is not on PATH; add it to your shell profile" >&2 ;;
esac
echo "bale: next, run 'bale install' and 'bale install --hooks claude'"
