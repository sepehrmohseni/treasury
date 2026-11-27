#!/usr/bin/env python3
"""Fills the project page's downloads from the latest release.

usage: scripts/site-downloads.py RELEASES_JSON INDEX_HTML

RELEASES_JSON is GitHub's list of releases (`gh api --paginate --slurp
repos/OWNER/REPO/releases`), as one list or as a list of pages, or an empty file when there
are none, in which case the page keeps its "not out yet" text. The Pages workflow runs this
when it builds the site, so every link names a file that exists.

It fills the release line at the top and the download section: one choice per system and, for
each, the package that suits most people with the other formats beside it. site/download.js
then picks the system the visitor's browser reports. Older versions are on GitHub's releases
page.
"""

import html
import json
import re
import sys

# Each platform: the id the page and site/download.js know it by, its name, the detail beside
# it, its packages as (file name ending, label), and a note. The first package a release has is
# the one offered first.
PLATFORMS = [
    (
        "windows",
        "Windows",
        "x64",
        [("_x64-setup.exe", "Installer (.exe)"), ("_x64_en-US.msi", "Windows Installer (.msi)")],
        "The .exe installs for you alone, so updates need no administrator. The .msi installs "
        "for every user of the computer.",
    ),
    (
        "macos",
        "macOS",
        "Apple silicon and Intel",
        [("_universal.dmg", "Disk image (.dmg)")],
        "One app for both kinds of Mac.",
    ),
    (
        "linux-x64",
        "Linux",
        "x64",
        [("_amd64.AppImage", "AppImage"), ("_amd64.deb", ".deb"), (".x86_64.rpm", ".rpm")],
        "The AppImage updates itself without a password. The .deb and .rpm install through your "
        "package manager.",
    ),
    (
        "linux-arm64",
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
    """For each platform the release has packages for: its id, name, detail, note and the
    packages as (label, asset), the one offered first leading."""
    assets = release.get("assets", [])
    out = []
    for key, name, detail, packages, note in PLATFORMS:
        found = []
        for ending, label in packages:
            for asset in sorted(assets, key=lambda a: a["name"]):
                if asset["name"].endswith(ending):
                    found.append((label, asset))
        if found:
            out.append((key, name, detail, note, found))
    return out


def asset_named(release, name):
    return next((a for a in release.get("assets", []) if a["name"] == name), None)


def render_downloads(release, all_releases_url):
    platforms = platform_files(release)
    if not platforms:
        sys.exit(f"release {release['tag_name']} has none of the expected packages")
    choices = []
    panels = []
    for i, (key, name, detail, note, found) in enumerate(platforms):
        where = name if key in ("windows", "macos") else f"{name} {detail}"
        checked = " checked" if i == 0 else ""
        choices.append(
            f'        <input type="radio" name="os" id="os-{key}" value="{key}"{checked}>'
            f'<label for="os-{key}">{esc(where)}</label>'
        )
        (first_label, first), rest = found[0], found[1:]
        others = ", ".join(
            f'<a href="{esc(a["browser_download_url"])}">{esc(label)}'
            f'<span class="visually-hidden"> for {esc(where)}</span></a> '
            f'<span class="size">{megabytes(a["size"])}</span>'
            for label, a in rest
        )
        also = f'\n          <p class="also">Also as {others}</p>' if rest else ""
        panels.append(
            f'        <section class="platform" data-os="{key}" aria-label="{esc(name)} {esc(detail)}">\n'
            f'          <h3>{esc(name)} <span class="detail">{esc(detail)}</span></h3>\n'
            f'          <a class="get" href="{esc(first["browser_download_url"])}">'
            f"Download for {esc(where)}"
            f' <span class="what">{esc(first_label)}, {megabytes(first["size"])}</span></a>{also}\n'
            f'          <p class="note">{esc(note)}</p>\n'
            f"        </section>"
        )
    sums = asset_named(release, "SHA256SUMS")
    links = [f'<a href="{esc(release["html_url"])}">Release notes and every file</a>']
    if sums:
        links.append(f'<a href="{esc(sums["browser_download_url"])}">SHA256SUMS</a>')
    links.append(f'<a href="{esc(all_releases_url)}">Older versions</a>')
    return "\n".join(
        [
            f'    <p class="release-head"><span class="pill">Latest</span> Version '
            f'{esc(version(release))}, released <span class="when">{esc(date(release))}</span></p>',
            '    <div class="os-picker">',
            '      <fieldset class="os-tabs">',
            '        <legend class="visually-hidden">Your system</legend>',
            *choices,
            "      </fieldset>",
            '      <p class="os-note" id="os-picked" hidden>Picked for the system your browser '
            "reports. Choose another above if you need a different one.</p>",
            '      <p class="os-note" id="os-phone" hidden>There is no app for phones and tablets '
            "yet. These downloads are for Windows, macOS and Linux computers.</p>",
            '      <div class="os-panels">',
            *panels,
            "      </div>",
            "    </div>",
            f'    <p class="status release-links">{" ".join(links)}</p>',
        ]
    )


def render_release_line(release):
    return (
        f'      <p class="status"><span class="pill">Version {esc(version(release))}</span> '
        f'Released {esc(date(release))}. <a href="{esc(release["html_url"])}">What changed</a> '
        f'<a href="#download">Download</a></p>'
    )


def build(page, releases, all_releases_url):
    """`page` with its downloads filled from the latest of `releases`, or unchanged when there
    is none."""
    newest = latest(releases)
    if newest is None:
        return page
    page = fill(page, "release", render_release_line(newest))
    page = fill(page, "downloads", render_downloads(newest, all_releases_url))
    return page


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    releases_path, page_path = sys.argv[1:]
    releases = load_releases(open(releases_path, encoding="utf-8").read())
    newest = latest(releases)
    if newest is None:
        print("no release yet; the page keeps its placeholder")
        return
    # The releases page is the release's own URL without `/tag/<tag>`.
    all_releases = newest["html_url"].rsplit("/tag/", 1)[0]
    page = open(page_path, encoding="utf-8").read()
    page = build(page, releases, all_releases)
    with open(page_path, "w", encoding="utf-8") as f:
        f.write(page)
    print(f"filled the downloads of version {version(newest)}")


if __name__ == "__main__":
    main()
