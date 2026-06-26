# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2acli < Formula
  desc "Standalone A2A CLI client"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.6"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "15e6deba9687e6d967409e22b1adaa726012f30014a6ada637f102ec6534eaff"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "7e6878bf6b2e4552a34f656fe511b459d3cc22a7765349d954e3ae87b88036ae"
    end
  end

  def install
    bin.install "a2acli"
  end

  test do
    assert_match "a2acli", shell_output("#{bin}/a2acli --help")
  end
end
