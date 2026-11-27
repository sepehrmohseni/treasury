#!/usr/bin/env python3
"""Fills the project page's downloads from the published releases.

usage: scripts/site-downloads.py RELEASES_JSON INDEX_HTML

RELEASES_JSON is GitHub's list of releases (`gh api --paginate --slurp
repos/OWNER/REPO/releases`), as one list or as a list of pages, or an empty file when there
are none, in which case the page keeps its "not out yet" text. The Pages workflow runs this
when it builds the site, so every link names a file that exists.

It fills three places: the release line at the top, one card per platform for the latest
version, and the list of every version with its files.
"""

import html
import json
import re
import sys

# Each platform: its heading, the detail under it, and its packages as (file name ending,
# label). The first package a release has is the one the card offers first.
PLATFORMS = [
    (
        "Windows",
        "x64",
        [("_x64-setup.exe", "Installer (.exe)"), ("_x64_en-US.msi", "Windows Installer (.msi)")],
        "The .exe installs for you alone, so updates need no administrator. The .msi installs "
        "for every user of the computer.",
    ),
    (
        "macOS",
        "Apple silicon and Intel",
        [("_universal.dmg", "Disk image (.dmg)")],
        "One app for both kinds of Mac.",
    ),
    (
        "Linux",
        "x64",
        [("_amd64.AppImage", "AppImage"), ("_amd64.deb", ".deb"), (".x86_64.rpm", ".rpm")],
        "The AppImage updates itself without a password. The .deb and .rpm install through your "
        "package manager.",
    ),
    (
        "Linux",
        "arm64",
        [("_aarch64.AppImage", "AppImage"), ("_arm64.deb", ".deb"), (".aarch64.rpm", ".rpm")],
        "The same packages, for 64-bit ARM.",
    ),
]

esc = html.escape


def megabytes(n):
    return f"{n / 1_000_000:.1f} MB"


def fill(page, name, body):
    pattern = re.compile(rf"(<!-- {name}:start -->\n).*?(\n\s*<!-- {name}:end -->)", re.S)
    if not pattern.search(page):
        sys.exit(f"the page has no {name} markers")
    return pattern.sub(lambda m: m.group(1) + body + m.group(2), page, count=1)


def load_releases(raw):
    """The published releases in `raw`, newest version first. Drafts are left out."""
    raw = raw.strip()
    if not raw:
        return []
    data = json.loads(raw)
    if isinstance(data, dict):
        data = [data]
    # `--slurp` wraps each page in a list of its own.
    if data and all(isinstance(page, list) for page in data):
        data = [r for page in data for r in page]
    releases = [r for r in data if isinstance(r, dict) and "tag_name" in r and not r.get("draft")]
    return sorted(releases, key=version_key, reverse=True)


def version_key(release):
    numbers = re.findall(r"\d+", release["tag_name"])
    return ([int(n) for n in numbers[:3]], release.get("published_at") or "")


def latest(releases):
    """The newest release that is not a pre-release, as GitHub marks the latest."""
    return next((r for r in releases if not r.get("prerelease")), None)


def version(release):
    return release["tag_name"].removeprefix("v")


def date(release):
    return (release.get("published_at") or "")[:10]


def platform_files(release):
    """For each platform the release has packages for: its heading, detail, note and the
    packages as (label, asset), the one offered first leading."""
    assets = release.get("assets", [])
    out = []
    for name, detail, packages, note in PLATFORMS:
        found = []
        for ending, label in packages:
            for asset in sorted(assets, key=lambda a: a["name"]):
                if asset["name"].endswith(ending):
                    found.append((label, asset))
        if found:
            out.append((name, detail, note, found))
    return out


def asset_named(release, name):
    return next((a for a in release.get("assets", []) if a["name"] == name), None)


def render_cards(release):
    cards = []
    for name, detail, note, found in platform_files(release):
        (first_label, first), rest = found[0], found[1:]
        where = f"{name} {detail}"
        others = "".join(
            f'\n            <li><a href="{esc(a["browser_download_url"])}">{esc(label)}'
            f'<span class="visually-hidden"> for {esc(where)}</span></a> '
            f'<span class="size">{megabytes(a["size"])}</span></li>'
            for label, a in rest
        )
        also = f'\n          <ul class="also">{others}\n          </ul>' if rest else ""
        cards.append(
            f'      <section class="platform" aria-label="{esc(where)}">\n'
            f'        <h3>{esc(name)} <span class="detail">{esc(detail)}</span></h3>\n'
            f'        <a class="get" href="{esc(first["browser_download_url"])}">{esc(first_label)}'
            f'<span class="visually-hidden"> for {esc(where)}</span>'
            f' <span class="size">{megabytes(first["size"])}</span></a>{also}\n'
            f'        <p class="note">{esc(note)}</p>\n'
            f"      </section>"
        )
    if not cards:
        sys.exit(f"release {release['tag_name']} has none of the expected packages")
    sums = asset_named(release, "SHA256SUMS")
    links = [f'<a href="{esc(release["html_url"])}">Release notes and every file</a>']
    if sums:
        links.append(f'<a href="{esc(sums["browser_download_url"])}">SHA256SUMS</a>')
    return "\n".join(
        [
            f'    <p class="release-head"><span class="pill">Latest</span> Version '
            f'{esc(version(release))}, released <span class="when">{esc(date(release))}</span></p>',
            '    <div class="platforms">',
            *cards,
            "    </div>",
            f'    <p class="status release-links">{" ".join(links)}</p>',
        ]
    )


def render_versions(releases, newest):
    rows = []
    for release in releases:
        pills = []
        if newest is not None and release["tag_name"] == newest["tag_name"]:
            pills.append('<span class="pill">Latest</span>')
        if release.get("prerelease"):
            pills.append('<span class="pill">Pre-release</span>')
        groups = []
        for name, detail, _note, found in platform_files(release):
            files = "".join(
                f'<li><a href="{esc(a["browser_download_url"])}">{esc(a["name"])}</a> '
                f'<span class="size">{megabytes(a["size"])}</span></li>'
                for _label, a in found
            )
            groups.append(
                f"          <div><dt>{esc(name)} {esc(detail)}</dt>"
                f'<dd><ul class="files">{files}</ul></dd></div>'
            )
        sums = asset_named(release, "SHA256SUMS")
        links = [f'<a href="{esc(release["html_url"])}">Release notes</a>']
        if sums:
            links.append(f'<a href="{esc(sums["browser_download_url"])}">SHA256SUMS</a>')
        body = (
            '\n        <dl class="version-files">\n' + "\n".join(groups) + "\n        </dl>"
            if groups
            else '\n        <p class="status">This version has no packages.</p>'
        )
        rows.append(
            '      <details class="version">\n'
            f'        <summary><span class="number">{esc(version(release))}</span>'
            f'{"".join(pills)}<span class="date">{esc(date(release))}</span></summary>'
            f"{body}\n"
            f'        <p class="status release-links">{" ".join(links)}</p>\n'
            "      </details>"
        )
    return "\n".join(
        [
            '    <div class="versions-head">',
            '      <h3 id="versions-title">All versions</h3>',
            "      <p>Installed copies update themselves when a new version is out. These are for "
            "installing a particular version again.</p>",
            "    </div>",
            '    <div class="versions" aria-labelledby="versions-title">',
            *rows,
            "    </div>",
        ]
    )


def render_release_line(release):
    return (
        f'      <p class="status"><span class="pill">Version {esc(version(release))}</span> '
        f'Released {esc(date(release))}. <a href="{esc(release["html_url"])}">What changed</a> '
        f'<a href="#download">Download</a></p>'
    )


def build(page, releases):
    """`page` with its downloads filled from `releases`, or unchanged when there are none."""
    newest = latest(releases)
    if newest is None:
        return page
    page = fill(page, "release", render_release_line(newest))
    page = fill(page, "downloads", render_cards(newest))
    page = fill(page, "versions", render_versions(releases, newest))
    return page


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    releases_path, page_path = sys.argv[1:]
    releases = load_releases(open(releases_path, encoding="utf-8").read())
    if latest(releases) is None:
        print("no release yet; the page keeps its placeholder")
        return
    page = open(page_path, encoding="utf-8").read()
    page = build(page, releases)
    with open(page_path, "w", encoding="utf-8") as f:
        f.write(page)
    print(f"filled the downloads of {len(releases)} version(s), latest {version(latest(releases))}")


if __name__ == "__main__":
    main()
