// Picks the download for the system this browser reports. Nothing is sent anywhere: the page
// only reads what the browser says about itself. Without this script the page shows Windows
// first and every system can still be picked by hand.

"use strict";

// The download that fits: "windows", "macos", "linux-x64", "linux-arm64", or "phone" for
// phones and tablets, which have no app yet, or null when the system is not recognised.
function pickOs(platform, userAgent, touchPoints) {
  var s = (String(platform || "") + " " + String(userAgent || "")).toLowerCase();
  if (/android|iphone|ipad|ipod/.test(s)) return "phone";
  // iPads ask for desktop sites and then report a Mac; only they have a touch screen.
  if (/mac/.test(s)) return touchPoints > 1 ? "phone" : "macos";
  if (/win/.test(s)) return "windows";
  if (/linux|x11|cros/.test(s)) {
    return /aarch64|arm64|armv8/.test(s) ? "linux-arm64" : "linux-x64";
  }
  return null;
}

function choose(os) {
  var input = document.getElementById("os-" + os);
  if (!input) return false;
  input.checked = true;
  return true;
}

function start() {
  var ua = navigator.userAgentData;
  var os = pickOs(ua && ua.platform ? ua.platform : navigator.platform, navigator.userAgent,
    navigator.maxTouchPoints || 0);
  var note = document.getElementById(os === "phone" ? "os-phone" : "os-picked");
  if (os && os !== "phone" && !choose(os)) return;
  if (note && os) note.hidden = false;
  // Chromium on Linux tells the processor only when asked.
  if (os === "linux-x64" && ua && ua.getHighEntropyValues) {
    ua.getHighEntropyValues(["architecture"]).then(function (v) {
      if (v && v.architecture === "arm") choose("linux-arm64");
    }, function () {});
  }
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = { pickOs: pickOs };
} else if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", start);
} else {
  start();
}
