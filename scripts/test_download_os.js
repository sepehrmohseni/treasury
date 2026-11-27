// Tests for the project page's system detection (site/download.js). Run with
// `node --test scripts/test_download_os.js`; the Pages workflow runs them before it builds.

"use strict";

const test = require("node:test");
const assert = require("node:assert");
const { pickOs } = require("../site/download.js");

const cases = [
  ["Win32", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36", 0, "windows"],
  ["Windows", "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0", 0, "windows"],
  ["MacIntel", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15", 0, "macos"],
  ["macOS", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36", 0, "macos"],
  ["Linux x86_64", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36", 0, "linux-x64"],
  ["Linux aarch64", "Mozilla/5.0 (X11; Linux aarch64; rv:133.0) Gecko/20100101 Firefox/133.0", 0, "linux-arm64"],
  ["Linux", "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0", 0, "linux-x64"],
  ["Linux armv8l", "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Mobile Safari/537.36", 5, "phone"],
  ["iPhone", "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1", 5, "phone"],
  // An iPad asking for the desktop site says it is a Mac; its touch screen gives it away.
  ["MacIntel", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15", 5, "phone"],
  ["", "", 0, null],
  [undefined, "SomeBot/1.0", 0, null],
];

for (const [platform, ua, touch, want] of cases) {
  test(`${platform || "(none)"} with ${touch} touch points is ${want}`, () => {
    assert.strictEqual(pickOs(platform, ua, touch), want);
  });
}
