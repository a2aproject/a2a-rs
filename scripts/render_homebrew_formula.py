#!/usr/bin/env python3
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import argparse
from pathlib import Path
import re
import tomllib
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_MANIFEST = ROOT / "Cargo.toml"
CLI_MANIFEST = ROOT / "a2acli" / "Cargo.toml"
DEFAULT_OUTPUT = ROOT / "Formula" / "a2acli.rb"
TAG_PATTERN = re.compile(r"^a2a-cli-v(?P<version>[0-9A-Za-z.+-]+)$")
MACOS_ARM64_TARGET = "aarch64-apple-darwin"
MACOS_X86_64_TARGET = "x86_64-apple-darwin"
RELEASE_BASE_URL = "https://github.com/a2aproject/a2a-rs/releases/download"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Render the Homebrew formula for the released a2acli tag."
    )
    parser.add_argument(
        "--tag",
        required=True,
        help="Release tag in the form a2a-cli-v<version>",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help=f"Formula output path (default: {DEFAULT_OUTPUT})",
    )
    return parser.parse_args()


def load_toml(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def resolve_workspace_value(package: dict, workspace_package: dict, key: str) -> str:
    value = package.get(key)
    if isinstance(value, dict) and value.get("workspace"):
        return workspace_package[key]
    if value is None:
        return workspace_package[key]
    if not isinstance(value, str):
        raise SystemExit(f"expected {key} to resolve to a string")
    return value


def fetch_text(url: str) -> str:
    request = urllib.request.Request(url, headers={"User-Agent": "a2aproject-release-automation"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read().decode("utf-8")
    except urllib.error.URLError as error:
        reason = f"HTTP {error.code}" if isinstance(error, urllib.error.HTTPError) else error.reason
        raise SystemExit(f"failed to fetch {url}: {reason}") from error


def fetch_release_sha256(tag: str, version: str, target: str) -> str:
    checksum_url = f"{RELEASE_BASE_URL}/{tag}/a2acli-v{version}-{target}.tar.gz.sha256"
    text = fetch_text(checksum_url)
    parts = text.split()
    if not parts:
        raise SystemExit(f"empty checksum from {checksum_url}")
    sha = parts[0].lower()
    return sha


def ruby_string(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def render_formula(tag: str) -> str:
    match = TAG_PATTERN.match(tag)
    if not match:
        raise SystemExit(f"expected a2a-cli-v<version> tag, got: {tag}")
    version = match.group("version")

    workspace = load_toml(WORKSPACE_MANIFEST)
    cli_manifest = load_toml(CLI_MANIFEST)
    workspace_package = workspace["workspace"]["package"]
    package = cli_manifest["package"]

    description = package["description"]
    homepage = resolve_workspace_value(package, workspace_package, "repository")
    license_id = resolve_workspace_value(package, workspace_package, "license")

    arm64_sha256 = fetch_release_sha256(tag, version, MACOS_ARM64_TARGET)
    x86_64_sha256 = fetch_release_sha256(tag, version, MACOS_X86_64_TARGET)

    return (
        f"# Copyright AGNTCY Contributors (https://github.com/agntcy)\n"
        f"# SPDX-License-Identifier: Apache-2.0\n"
        f"\n"
        f'class A2acli < Formula\n'
        f'  desc "{ruby_string(description)}"\n'
        f'  homepage "{ruby_string(homepage)}"\n'
        f'  version "{version}"\n'
        f'  license "{ruby_string(license_id)}"\n'
        f"  depends_on :macos\n"
        f"\n"
        f"  on_macos do\n"
        f"    on_arm do\n"
        f'      url "{RELEASE_BASE_URL}/a2a-cli-v#{{version}}/a2acli-v#{{version}}-{MACOS_ARM64_TARGET}.tar.gz"\n'
        f'      sha256 "{arm64_sha256}"\n'
        f"    end\n"
        f"\n"
        f"    on_intel do\n"
        f'      url "{RELEASE_BASE_URL}/a2a-cli-v#{{version}}/a2acli-v#{{version}}-{MACOS_X86_64_TARGET}.tar.gz"\n'
        f'      sha256 "{x86_64_sha256}"\n'
        f"    end\n"
        f"  end\n"
        f"\n"
        f"  def install\n"
        f'    bin.install "a2acli"\n'
        f"  end\n"
        f"\n"
        f"  test do\n"
        f'    assert_match "a2acli", shell_output("#{{bin}}/a2acli --help")\n'
        f"  end\n"
        f"end\n"
    )


def main() -> int:
    args = parse_args()
    formula = render_formula(args.tag)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(formula, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
