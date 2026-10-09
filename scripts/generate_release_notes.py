#!/usr/bin/env python3
"""Generate Stremio Native release notes from assets, changelog, and git history."""

from __future__ import annotations

import argparse
import fnmatch
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable
from urllib.parse import quote


PROJECT_DESCRIPTION = (
    "A high-performance native Stremio desktop client with a Slint interface, "
    "embedded Rust stream server, and hardware-accelerated libmpv playback."
)

ASSET_PATTERNS = (
    ("StremioSetup-v*-x64.exe", "Windows installer"),
    (
        "stremio-native-v*-x86_64-pc-windows-msvc.zip",
        "Windows automatic updater",
    ),
    (
        "stremio-native-v*-x86_64-unknown-linux-gnu",
        "Linux x64 portable",
    ),
    (
        "stremio-native-v*-x86_64-v2-unknown-linux-gnu",
        "Linux x64 portable (legacy CPUs without AVX2)",
    ),
    ("*.deb", "Debian / Ubuntu Package"),
    ("*.pkg.tar.zst", "Arch Linux Package"),
    ("*.rpm", "Fedora / RedHat RPM Package"),
    ("SHA256SUMS.txt", "SHA-256 checksums"),
)

CATEGORY_ORDER = (
    "Features & Enhancements",
    "Bug Fixes",
    "Performance",
    "UI",
    "Playback",
    "Storage",
    "Documentation",
    "CI & Build",
    "Maintenance",
    "Other Changes",
)

TYPE_CATEGORIES = {
    "feat": "Features & Enhancements",
    "feature": "Features & Enhancements",
    "fix": "Bug Fixes",
    "bugfix": "Bug Fixes",
    "perf": "Performance",
    "ui": "UI",
    "style": "UI",
    "docs": "Documentation",
    "doc": "Documentation",
    "ci": "CI & Build",
    "build": "CI & Build",
    "release": "CI & Build",
    "refactor": "Maintenance",
    "chore": "Maintenance",
    "test": "Maintenance",
}

SCOPE_CATEGORIES = {
    "mpv": "Playback",
    "playback": "Playback",
    "player": "Playback",
    "ui": "UI",
    "slint": "UI",
    "storage": "Storage",
    "cache": "Storage",
    "ci": "CI & Build",
    "release": "CI & Build",
}

CONVENTIONAL_RE = re.compile(
    r"^(?P<type>[A-Za-z]+)(?:\((?P<scope>[^)]+)\))?(?:!)?:\s*(?P<message>.+)$"
)


@dataclass(frozen=True)
class Commit:
    sha: str
    subject: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="GitHub repository as owner/name")
    parser.add_argument("--tag", required=True, help="Release tag being generated")
    parser.add_argument(
        "--previous-tag",
        default="",
        help="Previous release tag; empty means this is the initial release",
    )
    parser.add_argument("--asset-dir", required=True, type=Path)
    parser.add_argument("--changelog", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def git_log(previous_tag: str, tag: str) -> list[Commit]:
    if not previous_tag:
        return []

    result = subprocess.run(
        ["git", "log", f"{previous_tag}..{tag}", "--format=%H%x1f%s"],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    commits = []
    for line in result.stdout.splitlines():
        sha, separator, subject = line.partition("\x1f")
        if separator and sha and subject:
            commits.append(Commit(sha=sha, subject=subject))
    return commits


def strip_conventional_prefix(subject: str) -> str:
    match = CONVENTIONAL_RE.match(subject)
    return match.group("message").strip() if match else subject.strip()


def categorize(subject: str) -> str:
    match = CONVENTIONAL_RE.match(subject)
    if not match:
        return "Other Changes"

    scope = (match.group("scope") or "").lower()
    if scope in SCOPE_CATEGORIES:
        return SCOPE_CATEGORIES[scope]
    return TYPE_CATEGORIES.get(match.group("type").lower(), "Other Changes")


def asset_label(name: str) -> str:
    for pattern, label in ASSET_PATTERNS:
        if fnmatch.fnmatchcase(name, pattern):
            return label
    return "Additional asset"


def sorted_assets(asset_dir: Path) -> list[Path]:
    def sort_key(path: Path) -> tuple[int, str]:
        for index, (pattern, _) in enumerate(ASSET_PATTERNS):
            if fnmatch.fnmatchcase(path.name, pattern):
                return index, path.name.lower()
        return len(ASSET_PATTERNS), path.name.lower()

    return sorted(
        (path for path in asset_dir.iterdir() if path.is_file()), key=sort_key
    )


def github_download_url(repo: str, tag: str, name: str) -> str:
    return (
        f"https://github.com/{repo}/releases/download/"
        f"{quote(tag, safe='')}/{quote(name, safe='')}"
    )


def github_commit_url(repo: str, sha: str) -> str:
    return f"https://github.com/{repo}/commit/{sha}"


def render_downloads(repo: str, tag: str, assets: Iterable[Path]) -> list[str]:
    lines = ["### Downloads", "", "| Platform | Download |", "| --- | --- |"]
    for asset in assets:
        lines.append(
            f"| {asset_label(asset.name)} | "
            f"[Download]({github_download_url(repo, tag, asset.name)}) |"
        )
    lines.extend(["", "Verify downloads against `SHA256SUMS.txt`.", "", "---", ""])
    return lines


def release_version(tag: str) -> str:
    return tag.strip().lstrip("vV")


def changelog_section(changelog: Path, tag: str) -> list[str]:
    version = re.escape(release_version(tag))
    heading = re.compile(rf"^##\s+{version}(?:\s+-.*)?\s*$")
    lines = changelog.read_text(encoding="utf-8").splitlines()

    start = next((index for index, line in enumerate(lines) if heading.match(line)), None)
    if start is None:
        raise ValueError(f"CHANGELOG.md has no section for {release_version(tag)}")

    section = []
    for line in lines[start + 1 :]:
        if line.startswith("## "):
            break
        if match := re.match(r"^(#{3,5})(\s+.+)$", line):
            line = f"#{match.group(1)}{match.group(2)}"
        section.append(line)

    while section and not section[0].strip():
        section.pop(0)
    while section and not section[-1].strip():
        section.pop()
    if not section:
        raise ValueError(f"CHANGELOG.md section for {release_version(tag)} is empty")
    return section


def grouped_commits(commits: Iterable[Commit]) -> dict[str, list[Commit]]:
    grouped = {category: [] for category in CATEGORY_ORDER}
    for commit in commits:
        grouped[categorize(commit.subject)].append(commit)
    return grouped


def render_commit_changelog(
    repo: str, tag: str, previous_tag: str, commits: list[Commit]
) -> list[str]:
    if not previous_tag:
        return [
            "### Source",
            "",
            f"[Browse the {tag} source](https://github.com/{repo}/tree/{quote(tag, safe='')})",
        ]

    lines = [f"### Changes since {previous_tag}", ""]
    grouped = grouped_commits(commits)
    for category in CATEGORY_ORDER:
        entries = grouped[category]
        if not entries:
            continue
        lines.extend([f"#### {category}", ""])
        for commit in entries:
            short = commit.sha[:7]
            message = strip_conventional_prefix(commit.subject)
            lines.append(
                f"- {message} ([{short}]({github_commit_url(repo, commit.sha)}))"
            )
        lines.append("")

    lines.append(
        f"**Full comparison**: https://github.com/{repo}/compare/"
        f"{quote(previous_tag, safe='')}...{quote(tag, safe='')}"
    )
    return lines


def generate_release_notes(
    repo: str,
    tag: str,
    previous_tag: str,
    asset_dir: Path,
    changelog: Path,
) -> str:
    lines = [f"## Stremio Native {tag}", "", PROJECT_DESCRIPTION, ""]
    lines.extend(render_downloads(repo, tag, sorted_assets(asset_dir)))
    lines.extend(["### Release notes", ""])
    lines.extend(changelog_section(changelog, tag))
    lines.extend(["", "---", ""])
    lines.extend(render_commit_changelog(repo, tag, previous_tag, git_log(previous_tag, tag)))
    return "\n".join(lines).rstrip() + "\n"


def main() -> None:
    args = parse_args()
    body = generate_release_notes(
        repo=args.repo,
        tag=args.tag,
        previous_tag=args.previous_tag.strip(),
        asset_dir=args.asset_dir,
        changelog=args.changelog,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(body, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
