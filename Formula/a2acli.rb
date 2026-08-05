# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2acli < Formula
  desc "Standalone A2A CLI client"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.8"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "05e9d0c1be816c7bc2c720dda62e507e5875f627727dc0d50c68fbda6364e98f"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "fd36f14fba844e08f6ac4545747714cf7756f1eb331c849368a7cfb7fe657fa4"
    end
  end

  def install
    bin.install "a2acli"
  end

  test do
    assert_match "a2acli", shell_output("#{bin}/a2acli --help")
  end
end
