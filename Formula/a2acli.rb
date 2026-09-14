# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2acli < Formula
  desc "Standalone A2A CLI client"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.11"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "482e020b050a5109aead39236c4cc3bb4d00724dcdda33bda3c3cd77806884ff"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "3a4dcbfde58420f193a0d5d5a0c98fc523394e6b96a33f75bfd08a7f96dc22be"
    end
  end

  def install
    bin.install "a2acli"
  end

  test do
    assert_match "a2acli", shell_output("#{bin}/a2acli --help")
  end
end
