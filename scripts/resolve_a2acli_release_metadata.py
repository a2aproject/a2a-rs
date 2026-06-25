#!/usr/bin/env python3
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import argparse
from pathlib import Path
import tomllib


ROOT = Path(__file__).resolve().parent.parent
CLI_MANIFEST = ROOT / "a2acli" / "Cargo.toml"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Resolve a2acli release metadata for the GitHub Actions workflow."
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
        "--target",
        required=True,
        help="Rust target triple used to build a2acli.",
    )
    parser.add_argument(
        "--github-output",
        type=Path,
        required=True,
        help="GitHub Actions output file path.",
    )
    return parser.parse_args()


def load_version() -> str:
    with CLI_MANIFEST.open("rb") as handle:
        manifest = tomllib.load(handle)
    return manifest["package"]["version"]


def resolve_release_tag(event_name: str, release_tag: str, version: str) -> str:
    if event_name != "release":
        return ""

    prefix = "a2a-cli-v"
    if not release_tag.startswith(prefix):
        raise SystemExit(f"expected {prefix}<version>, got {release_tag}")

    tag_version = release_tag[len(prefix):]
    if tag_version != version:
        raise SystemExit(
            f"release tag version {tag_version} does not match a2acli/Cargo.toml version {version}"
        )

    return release_tag


def resolve_binary_path(target: str) -> str:
    binary_path = f"target/{target}/release/a2acli"
    if target.endswith("windows-msvc"):
        return f"{binary_path}.exe"
    return binary_path


def main() -> None:
    args = parse_args()

    version = load_version()
    release_tag = resolve_release_tag(args.event_name, args.release_tag, version)

    outputs = {
        "version": version,
        "release_tag": release_tag,
        "binary_path": resolve_binary_path(args.target),
    }

    with args.github_output.open("a", encoding="utf-8") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={value}\n")


if __name__ == "__main__":
    main()
