#!/usr/bin/env python3
# Copyright AGNTCY Contributors (https://github.com/agntcy)
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
from dataclasses import dataclass
from pathlib import Path
import tomllib
import urllib.request
from urllib.error import HTTPError, URLError


ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_MANIFEST = ROOT / "Cargo.toml"
CLI_MANIFEST = ROOT / "a2acli" / "Cargo.toml"
DEFAULT_OUTPUT_DIR = ROOT / "dist" / "winget"

MANIFEST_VERSION = "1.12.0"
PACKAGE_IDENTIFIER = "a2aproject.a2acli"
PACKAGE_LOCALE = "en-US"
PUBLISHER = "a2aproject"
PACKAGE_NAME = "a2acli"
MONIKER = "a2acli"
WINDOWS_ARCHITECTURE = "x64"
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
TAG_PATTERN = re.compile(r"^a2a-cli-v(?P<version>[0-9A-Za-z.+-]+)$")
REPOSITORY_PATTERN = re.compile(
    r"^https://github\.com/(?P<owner>[^/]+)/(?P<repo>[^/]+?)(?:\.git)?/?$"
)


@dataclass(frozen=True)
class ReleaseAssets:
    tag: str
    version: str
    release_date: str
    release_url: str
    installer_url: str
    installer_sha256: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Render WinGet manifests for a released a2acli tag."
    )
    parser.add_argument(
        "--tag",
        required=True,
        help="Release tag in the form a2a-cli-v<version>",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help=f"Output root for generated manifests (default: {DEFAULT_OUTPUT_DIR}).",
    )
    parser.add_argument(
        "--github-output",
        type=Path,
        help="Optional GitHub Actions output file to append manifest metadata to.",
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


def github_headers() -> dict[str, str]:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "a2aproject-release-automation",
    }
    token = None
    for env_var in ("GITHUB_TOKEN", "GH_TOKEN"):
        if env_var in os.environ and os.environ[env_var]:
            token = os.environ[env_var]
            break
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def fetch_bytes(url: str) -> bytes:
    request = urllib.request.Request(url, headers=github_headers())
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read()
    except URLError as error:
        reason = f"HTTP {error.code}" if isinstance(error, HTTPError) else error.reason
        raise SystemExit(f"failed to fetch {url}: {reason}") from error


def fetch_json(url: str) -> dict:
    return json.loads(fetch_bytes(url))


def fetch_text(url: str) -> str:
    return fetch_bytes(url).decode("utf-8")


def sha256_for_url(url: str) -> str:
    request = urllib.request.Request(url, headers=github_headers())
    digest = hashlib.sha256()
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            while True:
                chunk = response.read(1024 * 1024)
                if not chunk:
                    break
                digest.update(chunk)
    except URLError as error:
        reason = f"HTTP {error.code}" if isinstance(error, HTTPError) else error.reason
        raise SystemExit(f"failed to fetch {url}: {reason}") from error
    return digest.hexdigest().upper()


def parse_repository_slug(repository_url: str) -> str:
    match = REPOSITORY_PATTERN.match(repository_url)
    if not match:
        raise SystemExit(f"expected GitHub repository URL, got: {repository_url}")
    return f"{match.group('owner')}/{match.group('repo')}"


def parse_release_tag(tag: str) -> str:
    match = TAG_PATTERN.match(tag)
    if not match:
        raise SystemExit(f"expected a2a-cli-v<version> tag, got: {tag}")
    return match.group("version")


def resolve_release_assets(tag: str, repository_slug: str) -> ReleaseAssets:
    version = parse_release_tag(tag)
    release = fetch_json(
        f"https://api.github.com/repos/{repository_slug}/releases/tags/{tag}"
    )

    installer_name = f"a2acli-v{version}-{WINDOWS_TARGET}.zip"
    checksum_name = f"{installer_name}.sha256"

    installer_url = None
    checksum_url = None
    for asset in release.get("assets", []):
        asset_name = asset.get("name")
        asset_url = asset.get("browser_download_url")
        if asset_name == installer_name:
            installer_url = asset_url
        elif asset_name == checksum_name:
            checksum_url = asset_url

    if installer_url is None:
        raise SystemExit(
            f"release {tag} does not contain the expected Windows asset {installer_name}"
        )

    if checksum_url is not None:
        checksum_text = fetch_text(checksum_url)
        checksum_lines = [line.strip() for line in checksum_text.splitlines() if line.strip()]
        if not checksum_lines:
            raise SystemExit(f"checksum asset {checksum_name} is empty")
        installer_sha256 = checksum_lines[0].split()[0].upper()
    else:
        installer_sha256 = sha256_for_url(installer_url)

    release_date = release.get("published_at") or release.get("created_at")
    if release_date is None:
        raise SystemExit(f"release {tag} does not expose a publication date")

    return ReleaseAssets(
        tag=tag,
        version=version,
        release_date=release_date[:10],
        release_url=release["html_url"],
        installer_url=installer_url,
        installer_sha256=installer_sha256,
    )


def render_version_manifest(version: str) -> str:
    return (
        f"# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.{MANIFEST_VERSION}.schema.json\n"
        f"\n"
        f"PackageIdentifier: {PACKAGE_IDENTIFIER}\n"
        f"PackageVersion: {version}\n"
        f"DefaultLocale: {PACKAGE_LOCALE}\n"
        f"ManifestType: version\n"
        f"ManifestVersion: {MANIFEST_VERSION}\n"
    )


def render_default_locale_manifest(
    *,
    tag: str,
    version: str,
    description: str,
    repository_url: str,
    license_id: str,
) -> str:
    return (
        f"# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.{MANIFEST_VERSION}.schema.json\n"
        f"\n"
        f"PackageIdentifier: {PACKAGE_IDENTIFIER}\n"
        f"PackageVersion: {version}\n"
        f"PackageLocale: {PACKAGE_LOCALE}\n"
        f"Publisher: {PUBLISHER}\n"
        f"PublisherUrl: https://github.com/a2aproject\n"
        f"PublisherSupportUrl: {repository_url}/issues\n"
        f"Author: AGNTCY Contributors\n"
        f"PackageName: {PACKAGE_NAME}\n"
        f"PackageUrl: {repository_url}\n"
        f"License: {license_id}\n"
        f"LicenseUrl: {repository_url}/blob/{tag}/LICENSE.md\n"
        f"ShortDescription: {description}\n"
        f"Moniker: {MONIKER}\n"
        f"Tags:\n"
        f"- a2a\n"
        f"- agent\n"
        f"- cli\n"
        f"- llm\n"
        f"- protocol\n"
        f"ReleaseNotesUrl: {repository_url}/releases/tag/{tag}\n"
        f"ManifestType: defaultLocale\n"
        f"ManifestVersion: {MANIFEST_VERSION}\n"
    )


def render_installer_manifest(release_assets: ReleaseAssets) -> str:
    relative_file_path = (
        f"a2acli-v{release_assets.version}-{WINDOWS_TARGET}\\a2acli.exe"
    )
    return (
        f"# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.{MANIFEST_VERSION}.schema.json\n"
        f"\n"
        f"PackageIdentifier: {PACKAGE_IDENTIFIER}\n"
        f"PackageVersion: {release_assets.version}\n"
        f"InstallerType: zip\n"
        f"NestedInstallerType: portable\n"
        f"Commands:\n"
        f"- {MONIKER}\n"
        f"ReleaseDate: {release_assets.release_date}\n"
        f"Installers:\n"
        f"- Architecture: {WINDOWS_ARCHITECTURE}\n"
        f"  InstallerUrl: {release_assets.installer_url}\n"
        f"  InstallerSha256: {release_assets.installer_sha256}\n"
        f"  NestedInstallerFiles:\n"
        f"  - RelativeFilePath: {relative_file_path}\n"
        f"    PortableCommandAlias: {MONIKER}\n"
        f"ManifestType: installer\n"
        f"ManifestVersion: {MANIFEST_VERSION}\n"
    )


def manifest_directory(output_dir: Path, package_identifier: str, version: str) -> Path:
    identifier_parts = package_identifier.split(".")
    return output_dir / "manifests" / identifier_parts[0].lower()[0] / Path(*identifier_parts) / version


def write_github_output(path: Path, values: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8") as handle:
        for key, value in values.items():
            handle.write(f"{key}={value}\n")


def main() -> int:
    args = parse_args()

    workspace = load_toml(WORKSPACE_MANIFEST)
    cli_manifest = load_toml(CLI_MANIFEST)
    workspace_package = workspace["workspace"]["package"]
    package = cli_manifest["package"]

    repository_url = resolve_workspace_value(package, workspace_package, "repository")
    repository_slug = parse_repository_slug(repository_url)
    description = package["description"]
    license_id = resolve_workspace_value(package, workspace_package, "license")

    release_assets = resolve_release_assets(args.tag, repository_slug)

    output_dir = args.output_dir.resolve()
    manifests_dir = manifest_directory(output_dir, PACKAGE_IDENTIFIER, release_assets.version)
    if manifests_dir.exists():
        shutil.rmtree(manifests_dir)
    manifests_dir.mkdir(parents=True, exist_ok=True)

    (manifests_dir / f"{PACKAGE_IDENTIFIER}.yaml").write_text(
        render_version_manifest(release_assets.version),
        encoding="utf-8",
    )
    (manifests_dir / f"{PACKAGE_IDENTIFIER}.locale.{PACKAGE_LOCALE}.yaml").write_text(
        render_default_locale_manifest(
            tag=release_assets.tag,
            version=release_assets.version,
            description=description,
            repository_url=repository_url,
            license_id=license_id,
        ),
        encoding="utf-8",
    )
    (manifests_dir / f"{PACKAGE_IDENTIFIER}.installer.yaml").write_text(
        render_installer_manifest(release_assets),
        encoding="utf-8",
    )

    outputs = {
        "manifest_dir": str(manifests_dir),
        "manifest_rel_dir": manifests_dir.relative_to(output_dir).as_posix(),
        "package_identifier": PACKAGE_IDENTIFIER,
        "package_version": release_assets.version,
        "release_url": release_assets.release_url,
    }
    if args.github_output is not None:
        write_github_output(args.github_output, outputs)

    for key, value in outputs.items():
        print(f"{key}={value}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
