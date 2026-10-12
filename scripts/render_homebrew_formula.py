#!/usr/bin/env python3
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# Copyright A2A Contributors (https://github.com/a2aproject)
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import tomllib
import urllib.error
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_MANIFEST = ROOT / "Cargo.toml"
MACOS_ARM64_TARGET = "aarch64-apple-darwin"
MACOS_X86_64_TARGET = "x86_64-apple-darwin"
RELEASE_BASE_URL = "https://github.com/a2aproject/a2a-rs/releases/download"


@dataclass(frozen=True)
class Formula:
    tag_prefix: str
    manifest: Path
    class_name: str
    # Release archives are named `<binary>-v<version>-<target>.tar.gz`.
    binary: str
    output: Path
    test: str
    # Overrides the crate description when it is not a fit for `desc`.
    desc: str | None = None
    caveats: str | None = None


FORMULAE = (
    Formula(
        tag_prefix="a2a-cli-v",
        manifest=ROOT / "a2acli" / "Cargo.toml",
        class_name="A2acli",
        binary="a2acli",
        output=ROOT / "Formula" / "a2acli.rb",
        test='assert_match "a2acli", shell_output("#{bin}/a2acli --help")',
    ),
    Formula(
        tag_prefix="a2acli-transport-slimrpc-v",
        manifest=ROOT / "a2acli-transport-slimrpc" / "Cargo.toml",
        class_name="A2aTransportSlimrpc",
        binary="a2a-transport-slimrpc",
        output=ROOT / "Formula" / "a2a-transport-slimrpc.rb",
        test='assert_match "slimrpc", shell_output("#{bin}/a2a-transport-slimrpc info")',
        desc="SLIMRPC transport plugin for the Go A2A CLI",
        caveats=(
            "Set A2A_SLIMRPC_PLUGIN_CONFIG to a plugin YAML file, then run\n"
            "`a2a transport list` to check that the Go CLI finds `slimrpc`.\n"
            "https://github.com/a2aproject/a2a-rs/tree/main/a2acli-transport-slimrpc#readme"
        ),
    ),
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Render the Homebrew formula for a released binary tag."
    )
    parser.add_argument(
        "--tag",
        required=True,
        help="Release tag, e.g. a2a-cli-v<version> or a2acli-transport-slimrpc-v<version>",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="Formula output path (default: Formula/<name>.rb for the tag's package)",
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


def fetch_release_sha256(formula: Formula, tag: str, version: str, target: str) -> str:
    checksum_url = f"{RELEASE_BASE_URL}/{tag}/{formula.binary}-v{version}-{target}.tar.gz.sha256"
    text = fetch_text(checksum_url)
    parts = text.split()
    if not parts:
        raise SystemExit(f"empty checksum from {checksum_url}")
    sha = parts[0].lower()
    return sha


def ruby_string(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def select_formula(tag: str) -> tuple[Formula, str]:
    for formula in FORMULAE:
        if tag.startswith(formula.tag_prefix):
            version = tag[len(formula.tag_prefix):]
            if version and all(c.isalnum() or c in ".+-" for c in version):
                return formula, version
    expected = " or ".join(f"{f.tag_prefix}<version>" for f in FORMULAE)
    raise SystemExit(f"expected {expected} tag, got: {tag}")


def render_formula(tag: str) -> str:
    formula, version = select_formula(tag)

    workspace = load_toml(WORKSPACE_MANIFEST)
    manifest = load_toml(formula.manifest)
    workspace_package = workspace["workspace"]["package"]
    package = manifest["package"]

    description = formula.desc or package["description"]
    homepage = resolve_workspace_value(package, workspace_package, "repository")
    license_id = resolve_workspace_value(package, workspace_package, "license")

    arm64_sha256 = fetch_release_sha256(formula, tag, version, MACOS_ARM64_TARGET)
    x86_64_sha256 = fetch_release_sha256(formula, tag, version, MACOS_X86_64_TARGET)

    archive = f"{RELEASE_BASE_URL}/{formula.tag_prefix}#{{version}}/{formula.binary}-v#{{version}}"
    caveats = ""
    if formula.caveats:
        indented = "\n".join(f"      {line}" for line in formula.caveats.splitlines())
        caveats = (
            f"  def caveats\n"
            f"    <<~EOS\n"
            f"{indented}\n"
            f"    EOS\n"
            f"  end\n"
            f"\n"
        )

    return (
        f"# Copyright AGNTCY Contributors (https://github.com/agntcy)\n"
        f"# SPDX-License-Identifier: Apache-2.0\n"
        f"\n"
        f"class {formula.class_name} < Formula\n"
        f'  desc "{ruby_string(description)}"\n'
        f'  homepage "{ruby_string(homepage)}"\n'
        f'  version "{version}"\n'
        f'  license "{ruby_string(license_id)}"\n'
        f"  depends_on :macos\n"
        f"\n"
        f"  on_macos do\n"
        f"    on_arm do\n"
        f'      url "{archive}-{MACOS_ARM64_TARGET}.tar.gz"\n'
        f'      sha256 "{arm64_sha256}"\n'
        f"    end\n"
        f"\n"
        f"    on_intel do\n"
        f'      url "{archive}-{MACOS_X86_64_TARGET}.tar.gz"\n'
        f'      sha256 "{x86_64_sha256}"\n'
        f"    end\n"
        f"  end\n"
        f"\n"
        f"  def install\n"
        f'    bin.install "{formula.binary}"\n'
        f"  end\n"
        f"\n"
        f"{caveats}"
        f"  test do\n"
        f"    {formula.test}\n"
        f"  end\n"
        f"end\n"
    )


def main() -> int:
    args = parse_args()
    formula = render_formula(args.tag)
    output = args.output or select_formula(args.tag)[0].output
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(formula, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
