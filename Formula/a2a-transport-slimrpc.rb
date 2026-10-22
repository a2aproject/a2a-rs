# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2aTransportSlimrpc < Formula
  desc "SLIMRPC transport plugin for the Go A2A CLI"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.7"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "05a3ad86133d56653fd43eb07c22ee434d449bba5af4eff3f6b45a15a4b25796"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "3c28eb2c04f1ffec5834a00bf477e8b5a81d5090ad86412f9344fa04e6d3d716"
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
