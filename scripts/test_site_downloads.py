#!/usr/bin/env python3
"""Tests for site-downloads.py. Run with `python3 -I scripts/test_site_downloads.py`; the Pages
workflow runs them before it builds the page."""

import importlib.util
import json
import pathlib
import re
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
"""
RELEASES = "https://github.com/o/r/releases"

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
        return sd.build(PAGE, sd.load_releases(json.dumps(releases)), RELEASES)

    def panel(self, page, os):
        return page.split(f'data-os="{os}"')[1].split("</section>")[0]

    def test_each_system_has_a_choice_and_a_panel(self):
        page = self.built([release("0.1.1")])
        self.assertIn("Version 0.1.1", page)
        self.assertNotIn("not out yet", page)
        choices = re.findall(r'<input type="radio" name="os" id="os-([a-z0-9-]+)" value="\1"( checked)?>', page)
        self.assertEqual(
            choices,
            [("windows", " checked"), ("macos", ""), ("linux-x64", ""), ("linux-arm64", "")],
        )
        windows = self.panel(page, "windows")
        self.assertIn('class="get" href="https://github.com/o/r/releases/download/v0.1.1/Treasury_0.1.1_x64-setup.exe">Download for Windows', windows)
        self.assertIn("Treasury_0.1.1_x64_en-US.msi", windows)
        linux = self.panel(page, "linux-x64")
        self.assertIn('class="get" href="https://github.com/o/r/releases/download/v0.1.1/Treasury_0.1.1_amd64.AppImage">Download for Linux x64', linux)
        self.assertIn("Treasury_0.1.1_amd64.deb", linux)
        self.assertIn("14.0 MB", linux)
        self.assertIn(f'<a href="{RELEASES}">Older versions</a>', page)
        # Signatures and the update feed are for the updater, not for people.
        self.assertNotIn(".sig", page)
        self.assertNotIn("latest.json", page)
        self.assertIn("SHA256SUMS", page)

    def test_every_system_the_page_offers_has_its_style_rule(self):
        css = (HERE.parent / "site" / "style.css").read_text(encoding="utf-8")
        script = (HERE.parent / "site" / "download.js").read_text(encoding="utf-8")
        page = self.built([release("0.1.1")])
        for os in re.findall(r'id="os-([a-z0-9-]+)" value=', page):
            self.assertIn(f'.os-picker:has(#os-{os}:checked) .platform[data-os="{os}"]', css)
            self.assertIn(f'"{os}"', script)

    def test_only_the_latest_version_is_offered(self):
        page = self.built([release("0.1.1"), release("0.1.2"), release("0.2.0", prerelease=True)])
        self.assertIn("Treasury_0.1.2_universal.dmg", page)
        self.assertNotIn("Treasury_0.1.1_", page)
        self.assertNotIn("0.2.0", page)

    def test_missing_packages_leave_their_system_out(self):
        page = self.built([release("0.1.1", files=["Treasury_{v}_universal.dmg", "SHA256SUMS"])])
        self.assertEqual(page.count('<section class="platform"'), 1)
        self.assertIn('id="os-macos" value="macos" checked', page)
        with self.assertRaises(SystemExit):
            self.built([release("0.1.1", files=["SHA256SUMS"])])

    def test_names_are_escaped(self):
        r = release("0.1.1")
        r["assets"][0]["browser_download_url"] = 'https://example.com/a"b<c>'
        page = sd.build(PAGE, [r], RELEASES)
        self.assertIn("https://example.com/a&quot;b&lt;c&gt;", page)
        self.assertNotIn('a"b<c>', page)

    def test_no_release_leaves_the_page_alone(self):
        self.assertEqual(sd.build(PAGE, [], RELEASES), PAGE)


if __name__ == "__main__":
    unittest.main()
