#!/usr/bin/env python3
"""Marketplace pin-staleness gate (automatised-pipeline issue #67 (pattern: Cortex #179)).

Releasing a downstream plugin does not ship it: installs subscribe through
this repo's ``.claude-plugin/marketplace.json``, and delivery is gated by the
``version`` pinned there. This repo's own
manifest pinned 0.8.0 while its latest tag was v0.8.2 — the same defect as
Cortex #179, smaller lag, and only small by luck since nothing bounds it.
This gate makes that state loud.

Two failure classes (distinct on purpose — see zetetic-team-subagents#52:
a two-value check that only compares installed-vs-pin reports "up to date"
against a stale pin, a false compliance statement one level up):

  PIN_BEHIND_RELEASE  a github-sourced plugin's pin < that repo's latest
                      release tag — the release was never delivered.
  SELF_PIN_MISMATCH   a local ("./…") plugin's pin != its own plugin.json
                      version — the marketplace lies about this repo.

Exit codes: 0 all pins current, 1 stale pin(s), 2 usage/environment error.
Network: GitHub API only; sends Authorization when GITHUB_TOKEN is set
(required in CI to avoid the 60-req/h anonymous limit).
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.request
from pathlib import Path

MARKETPLACE = (
    Path(__file__).resolve().parent.parent / ".claude-plugin" / "marketplace.json"
)
API_TIMEOUT_S = 15  # source: matches the default urllib usage elsewhere in scripts/; GitHub p99 latency is far below this


def parse_semver(tag: str) -> tuple[int, ...] | None:
    """'v2.34.0' / '2.34.0' -> (2, 34, 0); None when the tag is not semver."""
    m = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)", tag.strip())
    if m is None:
        return None
    return tuple(int(g) for g in m.groups())


def latest_release_tag(repo: str) -> str:
    req = urllib.request.Request(
        f"https://api.github.com/repos/{repo}/releases/latest",
        headers=_headers(),
    )
    with urllib.request.urlopen(req, timeout=API_TIMEOUT_S) as resp:
        return json.load(resp)["tag_name"]


def _headers() -> dict[str, str]:
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "cortex-pin-gate"}
    token = os.environ.get("GITHUB_TOKEN", "")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def check_github_pin(
    name: str, repo: str, pin: str, fetch=latest_release_tag
) -> str | None:
    tag = fetch(repo)
    latest, pinned = parse_semver(tag), parse_semver(pin)
    if latest is None or pinned is None:
        return f"UNPARSEABLE: {name}: pin={pin!r} latest tag={tag!r} (non-semver; inspect manually)"
    if pinned < latest:
        return (
            f"PIN_BEHIND_RELEASE: {name}: marketplace pins {pin} but {repo} "
            f"latest release is {tag} — the release was never delivered to installs"
        )
    return None


def check_self_pin(name: str, source: str, pin: str, root: Path) -> str | None:
    plugin_json = (root / source / ".claude-plugin" / "plugin.json").resolve()
    if not plugin_json.is_file():
        plugin_json = (root / source / "plugin.json").resolve()
    if not plugin_json.is_file():
        return None  # local plugin without its own manifest (e.g. shim dirs) — pin is authoritative
    actual = json.loads(plugin_json.read_text()).get("version", "")
    if actual and actual != pin:
        return (
            f"SELF_PIN_MISMATCH: {name}: marketplace pins {pin} but "
            f"{plugin_json.relative_to(root)} says {actual}"
        )
    return None


def main() -> int:
    if not MARKETPLACE.is_file():
        print(f"ERROR: {MARKETPLACE} not found", file=sys.stderr)
        return 2
    root = MARKETPLACE.parent.parent
    data = json.loads(MARKETPLACE.read_text())
    failures: list[str] = []
    for plugin in data.get("plugins", []):
        name, pin, source = (
            plugin.get("name", "?"),
            plugin.get("version", ""),
            plugin.get("source"),
        )
        if not pin:
            continue
        if isinstance(source, dict) and source.get("source") == "github":
            issue = check_github_pin(name, source["repo"], pin)
        elif isinstance(source, str):
            issue = check_self_pin(name, source, pin, root)
        else:
            issue = None
        if issue:
            failures.append(issue)
    for f in failures:
        print(f)
    if failures:
        print(
            f"\n{len(failures)} stale pin(s). Bump .claude-plugin/marketplace.json — a release is not shipped until its pin moves."
        )
        return 1
    print("All marketplace pins current.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
