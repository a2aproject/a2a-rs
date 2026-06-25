#!/usr/bin/env python3
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import shutil
import tarfile
import zipfile


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUTPUT_DIR = ROOT / "dist"
DEFAULT_LICENSE = ROOT / "LICENSE.md"
DEFAULT_README = ROOT / "README.md"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Package a built a2acli binary into a release archive."
    )
    parser.add_argument(
        "--binary",
        type=Path,
        required=True,
        help="Path to the built a2acli binary.",
    )
    parser.add_argument(
        "--target",
        required=True,
        help="Rust target triple used to build the binary.",
    )
    parser.add_argument(
        "--version",
        required=True,
        help="CLI version used in the release archive name.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help=f"Directory for generated archives (default: {DEFAULT_OUTPUT_DIR}).",
    )
    parser.add_argument(
        "--license",
        type=Path,
        default=DEFAULT_LICENSE,
        help=f"License file to include in the archive (default: {DEFAULT_LICENSE}).",
    )
    parser.add_argument(
        "--readme",
        type=Path,
        default=DEFAULT_README,
        help=f"README file to include in the archive (default: {DEFAULT_README}).",
    )
    parser.add_argument(
        "--github-output",
        type=Path,
        help="Optional GitHub Actions output file to append archive metadata to.",
    )
    return parser.parse_args()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def write_github_output(path: Path, values: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8") as handle:
        for key, value in values.items():
            handle.write(f"{key}={value}\n")


def add_directory_to_zip(archive: Path, directory: Path) -> None:
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as handle:
        for path in sorted(directory.rglob("*")):
            if path.is_dir():
                continue
            handle.write(path, arcname=path.relative_to(directory.parent))


def add_directory_to_targz(archive: Path, directory: Path) -> None:
    with tarfile.open(archive, "w:gz") as handle:
        handle.add(directory, arcname=directory.name)


def main() -> None:
    args = parse_args()

    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"expected built binary at {binary}")

    for support_path in (args.license, args.readme):
        if not support_path.is_file():
            raise SystemExit(f"expected support file at {support_path}")

    archive_stem = f"a2acli-v{args.version}-{args.target}"
    output_dir = args.output_dir.resolve()
    staging_dir = output_dir / archive_stem
    output_dir.mkdir(parents=True, exist_ok=True)

    if staging_dir.exists():
        shutil.rmtree(staging_dir)
    staging_dir.mkdir()

    binary_name = "a2acli.exe" if args.target.endswith("windows-msvc") else "a2acli"
    shutil.copy2(binary, staging_dir / binary_name)
    shutil.copy2(args.license, staging_dir / args.license.name)
    shutil.copy2(args.readme, staging_dir / args.readme.name)

    if args.target.endswith("windows-msvc"):
        archive_path = output_dir / f"{archive_stem}.zip"
        add_directory_to_zip(archive_path, staging_dir)
    else:
        archive_path = output_dir / f"{archive_stem}.tar.gz"
        add_directory_to_targz(archive_path, staging_dir)

    checksum_path = output_dir / f"{archive_path.name}.sha256"
    checksum_path.write_text(
        f"{sha256(archive_path)}  {archive_path.name}\n",
        encoding="utf-8",
    )

    shutil.rmtree(staging_dir)

    outputs = {
        "archive_stem": archive_stem,
        "archive_path": str(archive_path),
        "checksum_path": str(checksum_path),
    }
    if args.github_output is not None:
        write_github_output(args.github_output, outputs)

    for key, value in outputs.items():
        print(f"{key}={value}")


if __name__ == "__main__":
    main()
