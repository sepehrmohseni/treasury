#!/usr/bin/env python3
"""Fills the project page's download table and release line from the latest release.

usage: scripts/site-downloads.py RELEASE_JSON INDEX_HTML

RELEASE_JSON is GitHub's reply for the latest release
(`gh api repos/OWNER/REPO/releases/latest`), or an empty file when there is none, in which case
the page keeps its "not out yet" text. The Pages workflow runs this when it builds the site, so
the links always name files that exist.
"""

import html
import json
import re
import sys

# Platform, then (file name ending, label) for each package, in the order they are listed.
PLATFORMS = [
    ("Linux x64", [("_amd64.AppImage", "AppImage"), ("_amd64.deb", "deb"), (".x86_64.rpm", "rpm")]),
    ("Linux arm64", [("_aarch64.AppImage", "AppImage"), ("_arm64.deb", "deb"), (".aarch64.rpm", "rpm")]),
    ("macOS, Apple silicon and Intel", [("_universal.dmg", "dmg")]),
    ("Windows x64", [("_x64-setup.exe", "installer"), ("_x64_en-US.msi", "msi")]),
]


def megabytes(n):
    return f"{n / 1_000_000:.1f} MB"


def fill(page, name, body):
    pattern = re.compile(rf"(<!-- {name}:start -->\n).*?(\n\s*<!-- {name}:end -->)", re.S)
    if not pattern.search(page):
        sys.exit(f"the page has no {name} markers")
    return pattern.sub(lambda m: m.group(1) + body + m.group(2), page, count=1)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    release_path, page_path = sys.argv[1:]
    raw = open(release_path, encoding="utf-8").read().strip()
    if not raw:
        print("no release yet; the page keeps its placeholder")
        return
    release = json.loads(raw)
    if "tag_name" not in release:
        print("no release yet; the page keeps its placeholder")
        return
    assets = {a["name"]: a for a in release.get("assets", [])}
    esc = html.escape
    rows = []
    for platform, packages in PLATFORMS:
        found = []
        for ending, label in packages:
            for name, asset in sorted(assets.items()):
                if name.endswith(ending):
                    found.append((label, asset))
        for i, (label, asset) in enumerate(found):
            head = f'<th scope="row" rowspan="{len(found)}">{esc(platform)}</th>' if i == 0 else ""
            rows.append(
                f'          <tr>{head}<td>{esc(label)}</td>'
                f'<td><a href="{esc(asset["browser_download_url"])}">{esc(asset["name"])}</a></td>'
                f'<td class="size">{megabytes(asset["size"])}</td></tr>'
            )
    if not rows:
        sys.exit("the latest release has none of the expected packages")
    version = release["tag_name"].removeprefix("v")
    notes = esc(release["html_url"])
    sums = assets.get("SHA256SUMS")
    sums_link = f'<a href="{esc(sums["browser_download_url"])}">SHA256SUMS</a>' if sums else ""
    table = "\n".join(
        [
            '    <div class="table-wrap">',
            '      <table class="status-table downloads">',
            '        <thead><tr><th scope="col">Platform</th><th scope="col">Package</th>'
            '<th scope="col">File</th><th scope="col">Size</th></tr></thead>',
            "        <tbody>",
            *rows,
            "        </tbody>",
            "      </table>",
            "    </div>",
            f'    <p class="status">Version {esc(version)}. Checksums: {sums_link}. '
            f'<a href="{notes}">Release notes and every file</a>.</p>',
        ]
    )
    date = esc(release.get("published_at", "")[:10])
    line = (
        f'      <p class="status"><span class="pill">Version {esc(version)}</span> Released {date}. '
        f'<a href="{notes}">What changed</a></p>'
    )
    page = open(page_path, encoding="utf-8").read()
    page = fill(page, "downloads", table)
    page = fill(page, "release", line)
    with open(page_path, "w", encoding="utf-8") as f:
        f.write(page)
    print(f"filled the downloads of version {version}")


if __name__ == "__main__":
    main()
