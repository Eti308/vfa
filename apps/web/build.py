#!/usr/bin/env python3
"""Build the VFA Encoder desktop app.

Produces in apps/web/dist/:
  VFA.html        single self-contained file (UI + codec as WebAssembly)
  VFA.app         macOS application bundle (opens VFA.html in its own window)
  VFA-mac.zip     both of the above plus a README, ready to copy to a Mac

Usage:  python3 build.py [--no-cargo]
(--no-cargo reuses an existing wasm build instead of running cargo.)
Only the Python standard library is needed.
"""
import base64
import os
import shutil
import stat
import struct
import subprocess
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
WASM_CRATE = os.path.join(ROOT, "apps", "wasm")
WASM = os.path.join(WASM_CRATE, "target", "wasm32-unknown-unknown", "release", "vfa_wasm.wasm")
DIST = os.path.join(HERE, "dist")
VERSION = "1.1"


def read(*parts):
    with open(os.path.join(HERE, *parts), encoding="utf-8") as f:
        return f.read()


def build_wasm():
    subprocess.run(
        ["cargo", "build", "--release", "--target", "wasm32-unknown-unknown"],
        cwd=WASM_CRATE,
        check=True,
    )


def build_html():
    with open(WASM, "rb") as f:
        wasm_b64 = base64.b64encode(f.read()).decode("ascii")
    html = read("src", "index.html")
    worker = read("src", "worker.js")
    app = read("src", "app.js")
    css = read("src", "style.css")
    for name, text in (("worker.js", worker), ("app.js", app)):
        if "</script" in text.lower():
            raise SystemExit(f"{name} must not contain a closing script tag")
    # replace placeholders one at a time (the wasm string goes last: it is large)
    html = html.replace("/*CSS*/", css, 1)
    html = html.replace("/*WORKER_JS*/", worker, 1)
    html = html.replace("/*APP_JS*/", app, 1)
    html = html.replace("/*WASM_B64*/", wasm_b64, 1)
    return html


def icns(pngs):
    """Assemble an .icns file from PNG data keyed by icns type code."""
    body = b""
    for code, data in pngs:
        body += code.encode("ascii") + struct.pack(">I", len(data) + 8) + data
    return b"icns" + struct.pack(">I", len(body) + 8) + body


INFO_PLIST = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>VFA Encoder</string>
  <key>CFBundleDisplayName</key><string>VFA Encoder</string>
  <key>CFBundleIdentifier</key><string>local.vfa.encoder</string>
  <key>CFBundleVersion</key><string>{VERSION}</string>
  <key>CFBundleShortVersionString</key><string>{VERSION}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>VFA</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSMinimumSystemVersion</key><string>10.13</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"""

LAUNCHER = """#!/bin/bash
# VFA Encoder launcher. The app itself is the self-contained page
# Resources/VFA.html (user interface + codec compiled to WebAssembly).
# It opens in a chromeless app window when Chrome, Edge, Brave or Chromium is
# installed, otherwise in the default browser (Safari works too).
# Everything runs locally; nothing is uploaded.
RES="$(cd "$(dirname "$0")/../Resources" && pwd)"
PAGE="$RES/VFA.html"
URL="file://${PAGE// /%20}"
for app in "Google Chrome" "Microsoft Edge" "Brave Browser" "Chromium" "Vivaldi"; do
  for dir in "/Applications" "$HOME/Applications"; do
    if [ -d "$dir/$app.app" ]; then
      open -na "$dir/$app.app" --args --app="$URL" --window-size=980,1000 && exit 0
    fi
  done
done
exec open "$PAGE"
"""

README = """VFA Encoder for macOS
=====================

Lossless, bit-exact encoder / decoder / player for float audio (.wav <-> .nx).
Everything runs locally on your Mac. Nothing is uploaded and nothing needs
to be installed.

Start it
--------
Option A, the app: double-click "VFA Encoder.app".
  It opens in its own window (Chrome/Edge/Brave/Chromium if installed,
  otherwise Safari).
  The first time, macOS may say the app "cannot be checked for malicious
  software" because it is not signed by an Apple developer. Then either
    - right-click the app -> Open -> Open (macOS 14 and earlier), or
    - System Settings -> Privacy & Security -> "Open Anyway" (macOS 15), or
    - in Terminal:  xattr -dr com.apple.quarantine "VFA Encoder.app"

Option B, no app: double-click VFA.html. It opens in your browser. Same
features, no security prompt.

Use it
------
- Drop a .wav or .nx file into the window (or Open... / Cmd-O).
- WAV: choose a level (0 fastest ... 5 default ... 8 smallest) and options,
  then "Encode to .nx". The .nx file is saved to your Downloads folder (or
  wherever you choose).
  With "Verify after encoding" each result is decoded again and compared bit
  for bit.
  Float32 WAVs are restored byte-identically. 8/16/24-bit WAVs are converted
  exactly to float32 first.
- NX: "Decode to .wav" restores the original WAV file byte for byte.
  Optionally decode only an excerpt (from / length in seconds).
- Player: play/pause (space bar), drag the waveform or the scrubber, +-10 s
  buttons, arrow keys +-5 s, loop, volume.
  Files above 0 dBFS are normalized for listening only; the files are never
  changed.
- Command line at the bottom: type the same options as the `vfa` CLI, e.g.
      --level 8 --no-stereo
      --block 4096 --fixed-blocks
      encode --level 2
      decode --start-time 30 --duration 10
      help
  Options typed there update the controls and vice versa. Up/down arrows
  recall previous commands.
"""


def write(path, data, mode=0o644):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb" if isinstance(data, bytes) else "w", **({} if isinstance(data, bytes) else {"encoding": "utf-8"})) as f:
        f.write(data)
    os.chmod(path, mode)


def main():
    if "--no-cargo" not in sys.argv:
        build_wasm()
    if os.path.exists(DIST):
        shutil.rmtree(DIST)
    os.makedirs(DIST)
    html = build_html()
    write(os.path.join(DIST, "VFA.html"), html)

    app = os.path.join(DIST, "VFA Encoder.app", "Contents")
    write(os.path.join(app, "Info.plist"), INFO_PLIST)
    write(os.path.join(app, "MacOS", "VFA"), LAUNCHER, 0o755)
    write(os.path.join(app, "Resources", "VFA.html"), html)
    pngs = []
    for code, size in (("ic07", 128), ("ic08", 256), ("ic09", 512), ("ic10", 1024)):
        with open(os.path.join(HERE, "assets", f"icon_{size}.png"), "rb") as f:
            pngs.append((code, f.read()))
    write(os.path.join(app, "Resources", "AppIcon.icns"), icns(pngs))
    write(os.path.join(DIST, "README.txt"), README)

    zpath = os.path.join(DIST, "VFA-mac.zip")
    with zipfile.ZipFile(zpath, "w", zipfile.ZIP_DEFLATED) as z:
        for base, dirs, files in os.walk(DIST):
            dirs.sort()
            for name in sorted(files):
                full = os.path.join(base, name)
                if full == zpath:
                    continue
                rel = os.path.join("VFA Encoder", os.path.relpath(full, DIST))
                info = zipfile.ZipInfo(rel, date_time=(2026, 1, 1, 0, 0, 0))
                mode = stat.S_IMODE(os.stat(full).st_mode)
                info.external_attr = (stat.S_IFREG | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                with open(full, "rb") as f:
                    z.writestr(info, f.read())
    print(f"VFA.html      {os.path.getsize(os.path.join(DIST, 'VFA.html')) / 1e3:.0f} KB")
    print(f"VFA-mac.zip   {os.path.getsize(zpath) / 1e3:.0f} KB  -> {zpath}")


if __name__ == "__main__":
    main()
