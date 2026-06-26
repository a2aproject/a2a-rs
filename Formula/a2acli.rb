# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

class A2acli < Formula
  desc "Standalone A2A CLI client"
  homepage "https://github.com/a2aproject/a2a-rs"
  url "https://github.com/a2aproject/a2a-rs/archive/refs/tags/a2a-cli-v0.1.6.tar.gz"
  sha256 "8cce1316e3f16ff072cfccbdd676e7f378805752768a5b54c039a9b774fcbbdc"
  license "Apache-2.0"
  head "https://github.com/a2aproject/a2a-rs.git", branch: "main"

  depends_on "cmake" => :build
  depends_on "rust" => :build

  def install
    system "cargo", "install", "--locked", "--path", "a2acli", *std_cargo_args
  end

  test do
    assert_match "a2acli", shell_output("#{bin}/a2acli --help")
  end
end
