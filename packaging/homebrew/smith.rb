class Smith < Formula
  desc "Terminal AI coding agent"
  homepage "https://github.com/pedro-canedo/smith"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/pedro-canedo/smith/releases/download/v#{version}/smith-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_APPLE_DARWIN_SHA256"
    else
      url "https://github.com/pedro-canedo/smith/releases/download/v#{version}/smith-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_APPLE_DARWIN_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/pedro-canedo/smith/releases/download/v#{version}/smith-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_LINUX_GNU_SHA256"
    else
      url "https://github.com/pedro-canedo/smith/releases/download/v#{version}/smith-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_X86_64_LINUX_GNU_SHA256"
    end
  end

  def install
    bin.install "smith"
  end

  test do
    system "#{bin}/smith", "--help"
  end
end
