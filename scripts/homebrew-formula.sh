#!/usr/bin/env bash
# Prints bale.rb for version $1 from the release archives' .sha256 files in
# directory $2.
set -euo pipefail
version="$1" dist="$2"
base="https://github.com/bmcreations/bale/releases/download/v$version"
sha() { cut -d' ' -f1 "$dist/bale-v$version-$1.tar.gz.sha256"; }
cat <<RUBY
class Bale < Formula
  desc "Keeps agent worktrees from filling your disk"
  homepage "https://github.com/bmcreations/bale"
  version "$version"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "$base/bale-v$version-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/bale-v$version-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$base/bale-v$version-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$(sha aarch64-unknown-linux-musl)"
    end
    on_intel do
      url "$base/bale-v$version-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$(sha x86_64-unknown-linux-musl)"
    end
  end

  def install
    bin.install "bale"
  end

  def caveats
    <<~EOS
      Start the sweep and hook your agent tools:
        bale install
        bale install --hooks claude
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/bale --version")
  end
end
RUBY
