# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2aTransportSlimrpc < Formula
  desc "SLIMRPC transport plugin for the Go A2A CLI"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.8"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "2a6d6d3ffb5bdcb4f72504c26394ffb9599c5dc962f13f45b0bb4dfd1eb60fa1"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "b494ae0d4a0e9404e9fa3e974088f4a7190698d8928f50a927d4328d66cfa9ec"
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
