# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2acli < Formula
  desc "Standalone A2A CLI client"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.4.1"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "7dbab1897ecbe2e0049d5a334fff225407ee9060f76df67c89833e83aef657d1"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "46358fa84ba27e9c994e630be7f5bd5132de1c1a127dd9556789f16bc049d3c4"
    end
  end

  def install
    bin.install "a2acli"
  end

  test do
    assert_match "a2acli", shell_output("#{bin}/a2acli --help")
  end
end
