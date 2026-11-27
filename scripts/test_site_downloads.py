#!/usr/bin/env python3
"""Tests for site-downloads.py. Run with `python3 -I scripts/test_site_downloads.py`; the Pages
workflow runs them before it builds the page."""

import importlib.util
import json
import pathlib
import unittest

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("site_downloads", HERE / "site-downloads.py")
sd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sd)

PAGE = """<p>top</p>
      <!-- release:start -->
      <p>No release yet.</p>
      <!-- release:end -->
    <!-- downloads:start -->
    <p>The first release is not out yet.</p>
    <!-- downloads:end -->
    <!-- versions:start -->
    <!-- none -->
    <!-- versions:end -->
"""

ALL_FILES = [
    "Treasury_{v}_x64-setup.exe",
    "Treasury_{v}_x64_en-US.msi",
    "Treasury_{v}_universal.dmg",
    "Treasury_{v}_amd64.AppImage",
    "Treasury_{v}_amd64.deb",
    "Treasury-{v}-1.x86_64.rpm",
    "Treasury_{v}_aarch64.AppImage",
    "Treasury_{v}_arm64.deb",
    "Treasury-{v}-1.aarch64.rpm",
    "Treasury_{v}_amd64.AppImage.sig",
    "latest.json",
    "SHA256SUMS",
]


def release(v, files=ALL_FILES, prerelease=False, draft=False, day="2026-11-27"):
    base = f"https://github.com/o/r/releases/download/v{v}/"
    return {
        "tag_name": f"v{v}",
        "html_url": f"https://github.com/o/r/releases/tag/v{v}",
        "published_at": f"{day}T08:51:36Z",
        "prerelease": prerelease,
        "draft": draft,
        "assets": [
            {"name": f.format(v=v), "size": 14_000_698, "browser_download_url": base + f.format(v=v)}
            for f in files
        ],
    }


class Releases(unittest.TestCase):
    def test_pages_are_joined_drafts_dropped_and_versions_ordered(self):
        raw = json.dumps(
            [[release("0.1.9"), release("0.2.0", draft=True)], [release("0.1.10")]]
        )
        got = [r["tag_name"] for r in sd.load_releases(raw)]
        self.assertEqual(got, ["v0.1.10", "v0.1.9"])
        self.assertEqual(sd.load_releases(""), [])
        self.assertEqual(len(sd.load_releases(json.dumps(release("0.1.1")))), 1)

    def test_the_latest_is_the_newest_that_is_not_a_pre_release(self):
        releases = sd.load_releases(
            json.dumps([release("0.2.0", prerelease=True), release("0.1.1")])
        )
        self.assertEqual(sd.latest(releases)["tag_name"], "v0.1.1")
        self.assertIsNone(sd.latest(sd.load_releases(json.dumps([release("0.2.0", prerelease=True)]))))


class Page(unittest.TestCase):
    def built(self, releases):
        return sd.build(PAGE, sd.load_releases(json.dumps(releases)))

    def test_each_platform_offers_its_first_package_and_lists_the_rest(self):
        page = self.built([release("0.1.1")])
        self.assertIn("Version 0.1.1", page)
        self.assertNotIn("not out yet", page)
        self.assertEqual(page.count('<section class="platform"'), 4)
        windows = page.split('aria-label="Windows x64"')[1].split("</section>")[0]
        self.assertIn('class="get" href="https://github.com/o/r/releases/download/v0.1.1/Treasury_0.1.1_x64-setup.exe"', windows)
        self.assertIn("Treasury_0.1.1_x64_en-US.msi", windows)
        linux = page.split('aria-label="Linux x64"')[1].split("</section>")[0]
        self.assertIn('class="get" href="https://github.com/o/r/releases/download/v0.1.1/Treasury_0.1.1_amd64.AppImage"', linux)
        self.assertIn(".deb", linux)
        self.assertIn("14.0 MB", linux)
        # Signatures and the update feed are for the updater, not for people.
        self.assertNotIn(".sig", page)
        self.assertIn("SHA256SUMS", page)

    def test_every_version_is_listed_newest_first_with_its_files(self):
        page = self.built([release("0.1.1"), release("0.1.2"), release("0.2.0", prerelease=True)])
        versions = page.split("versions:start")[1]
        order = [versions.index(f'<span class="number">{v}</span>') for v in ("0.2.0", "0.1.2", "0.1.1")]
        self.assertEqual(order, sorted(order))
        latest_row = versions.split('<span class="number">0.1.2</span>')[1].split("</summary>")[0]
        self.assertIn("Latest", latest_row)
        pre_row = versions.split('<span class="number">0.2.0</span>')[1].split("</summary>")[0]
        self.assertIn("Pre-release", pre_row)
        self.assertIn("Treasury_0.1.1_universal.dmg", versions)
        # The cards show the latest release, not the pre-release.
        cards = page.split("downloads:start")[1].split("downloads:end")[0]
        self.assertIn("Version 0.1.2", cards)
        self.assertNotIn("0.2.0", cards)

    def test_missing_packages_leave_their_platform_out(self):
        page = self.built([release("0.1.1", files=["Treasury_{v}_universal.dmg", "SHA256SUMS"])])
        self.assertEqual(page.count('<section class="platform"'), 1)
        empty = self.built([release("0.1.1"), release("0.1.0", files=["SHA256SUMS"])])
        self.assertIn("This version has no packages.", empty)
        with self.assertRaises(SystemExit):
            self.built([release("0.1.1", files=["SHA256SUMS"])])

    def test_names_are_escaped(self):
        r = release("0.1.1")
        r["assets"][0]["browser_download_url"] = 'https://example.com/a"b<c>'
        page = sd.build(PAGE, [r])
        self.assertIn("https://example.com/a&quot;b&lt;c&gt;", page)
        self.assertNotIn('a"b<c>', page)

    def test_no_release_leaves_the_page_alone(self):
        self.assertEqual(sd.build(PAGE, []), PAGE)


if __name__ == "__main__":
    unittest.main()
