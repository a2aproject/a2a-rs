# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2acli < Formula
  desc "Standalone A2A CLI client"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.2.1"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "1f32655fbadb6998c06e8e0926a2e1acefeb69a01732bda2f8f75372022df600"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2a-cli-v#{version}/a2acli-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "7eb7719ffa22c995a7d27fa2f2e2ec2dc1c50347806a932db2d5cc41cb074900"
    end
  end

  def install
    bin.install "a2acli"
  end

  test do
    assert_match "a2acli", shell_output("#{bin}/a2acli --help")
  end
end
