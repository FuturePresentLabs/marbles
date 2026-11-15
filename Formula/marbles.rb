# typed: false
# frozen_string_literal: true

# Tap this repository directly:
#   brew tap futurepresentlabs/marbles https://github.com/FuturePresentLabs/marbles
#   brew install futurepresentlabs/marbles/marbles
#
# It installs the published archive for the platform, so it tracks tagged
# releases rather than `main`. Bumping `version` and the three checksums is what
# a release costs here; the archives and their `.sha256` files come from
# .github/workflows/release.yml.
class Marbles < Formula
  desc "Small, single-writer issue tracker for fleets of coding agents"
  homepage "https://github.com/FuturePresentLabs/marbles"
  version "0.1.0"
  license "MIT"

  on_macos do
    # The release job builds macOS on macos-14, an ARM runner, and publishes no
    # x86-64-darwin archive.
    depends_on arch: :arm64
    on_arm do
      url "https://github.com/FuturePresentLabs/marbles/releases/download/v#{version}/marbles-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "96213bb9bee8f4bd1e8a7791cbe4573e15767dec6a2675adb12a144f7499dd2e"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/FuturePresentLabs/marbles/releases/download/v#{version}/marbles-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a04560c2ab00912ac34a24994e3bea259c5f96a18c593706c2d376b4990e4c65"
    end
    on_intel do
      url "https://github.com/FuturePresentLabs/marbles/releases/download/v#{version}/marbles-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ec5e1623d5f440ebcaf693800c91d3b2a066252cad22392af6a129ee0a16f13a"
    end
  end

  def install
    # The archive carries `mb` as a second name for the same binary; the native
    # CLI answers both.
    bin.install "marbles", "mb"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/marbles --version")
  end
end
