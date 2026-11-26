# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2aTransportSlimrpc < Formula
  desc "SLIMRPC transport plugin for the Go A2A CLI"
  homepage "https://github.com/a2aproject/a2a-rs"
  version "0.1.9"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    on_arm do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "9f6205283f849662f7d94dc6497a41d397301e4e97b5abdff3af38660c1c2fe5"
    end

    on_intel do
      url "https://github.com/a2aproject/a2a-rs/releases/download/a2acli-transport-slimrpc-v#{version}/a2a-transport-slimrpc-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "ce9ba072a96291f38372e40f86777fb009bf70c267719711de4c4efb0b9d58af"
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
