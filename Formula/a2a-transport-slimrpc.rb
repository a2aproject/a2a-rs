# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2aTransportSlimrpc < Formula
  desc "SLIMRPC transport plugin for the Go A2A CLI"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.3"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "a2c4ceaf657052dfdd7d3fd947e2c7c304553f955648f63f927d91efa0229ca3"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "c8974f452cd1d2aca0b32c33856a24c17de740be0d19e9aafdfad3e4331a70b4"
    end
  end

  def install
    bin.install "a2a-transport-slimrpc"
  end

  def caveats
    <<~EOS
      Set A2A_SLIMRPC_PLUGIN_CONFIG to a plugin YAML file, then run
      `a2a transport list` to check that the Go CLI finds `slimrpc`.
      https://github.com/a2aproject/a2a-rs/tree/main/a2acli-transport-slimrpc#readme
    EOS
  end

  test do
    assert_match "slimrpc", shell_output("#{bin}/a2a-transport-slimrpc info")
  end
end
