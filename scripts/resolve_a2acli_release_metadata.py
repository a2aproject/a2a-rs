#!/usr/bin/env python3
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# Copyright A2A Contributors (https://github.com/a2aproject)
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import tomllib


ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Package:
    cargo_package: str
    manifest: Path
    tag_prefix: str
    binary: str
    readme: Path


# One entry per crate that ships prebuilt binaries. `binary` is also the
# archive name, so release assets read `<binary>-v<version>-<target>`.
PACKAGES = {
    package.cargo_package: package
    for package in (
        Package(
            cargo_package="a2a-cli",
            manifest=ROOT / "a2acli" / "Cargo.toml",
            tag_prefix="a2a-cli-v",
            binary="a2acli",
            readme=ROOT / "README.md",
        ),
        Package(
            cargo_package="a2acli-transport-slimrpc",
            manifest=ROOT / "a2acli-transport-slimrpc" / "Cargo.toml",
            tag_prefix="a2acli-transport-slimrpc-v",
            binary="a2a-transport-slimrpc",
            readme=ROOT / "a2acli-transport-slimrpc" / "README.md",
        ),
    )
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Resolve release metadata for the GitHub Actions workflow."
    )
    parser.add_argument(
        "--event-name",
        required=True,
        help="GitHub event name for the current workflow run.",
    )
    parser.add_argument(
        "--release-tag",
        default="",
        help="GitHub release tag when the workflow is running for a published release.",
    )
    parser.add_argument(
        "--package",
        choices=sorted(PACKAGES),
        help="Crate to resolve. Defaults to the one the release tag names, else a2a-cli.",
    )
    parser.add_argument(
        "--target",
        required=True,
        help="Rust target triple the binary is built for.",
    )
    parser.add_argument(
        "--github-output",
        type=Path,
        required=True,
        help="GitHub Actions output file path.",
    )
    return parser.parse_args()


def select_package(name: str | None, release_tag: str) -> Package:
    if name:
        return PACKAGES[name]
    for package in PACKAGES.values():
        if release_tag.startswith(package.tag_prefix):
            return package
    return PACKAGES["a2a-cli"]


def load_version(package: Package) -> str:
    with package.manifest.open("rb") as handle:
        manifest = tomllib.load(handle)
    return manifest["package"]["version"]


def resolve_release_tag(event_name: str, release_tag: str, version: str, package: Package) -> str:
    if event_name != "release":
        return ""

    prefix = package.tag_prefix
    if not release_tag.startswith(prefix):
        raise SystemExit(f"expected {prefix}<version>, got {release_tag}")

    tag_version = release_tag[len(prefix):]
    if tag_version != version:
        manifest = package.manifest.relative_to(ROOT)
        raise SystemExit(
            f"release tag version {tag_version} does not match {manifest} version {version}"
        )

    return release_tag


def resolve_binary_path(target: str, binary: str) -> str:
    binary_path = f"target/{target}/release/{binary}"
    if target.endswith("windows-msvc"):
        return f"{binary_path}.exe"
    return binary_path


def main() -> None:
    args = parse_args()

    package = select_package(args.package, args.release_tag)
    version = load_version(package)
    release_tag = resolve_release_tag(args.event_name, args.release_tag, version, package)

    outputs = {
        "version": version,
        "release_tag": release_tag,
        "binary_path": resolve_binary_path(args.target, package.binary),
        "cargo_package": package.cargo_package,
        "archive_name": package.binary,
        "readme_path": str(package.readme.relative_to(ROOT)),
    }

    with args.github_output.open("a", encoding="utf-8") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={value}\n")


if __name__ == "__main__":
    main()
