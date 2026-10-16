# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2aTransportSlimrpc < Formula
  desc "SLIMRPC transport plugin for the Go A2A CLI"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.5"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "3d825f66c43ef6c67da317d1acdd00b84c6e612f9874a1daf1cd16d93c2dd8e7"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "6bf6baa5e4fea84d326e883dbbe57411f109cb0f341e7a219849dd054f2410e2"
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
