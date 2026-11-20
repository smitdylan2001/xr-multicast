#!/usr/bin/env python3
"""Regenerate THIRD_PARTY_NOTICES.md from the Cargo dependency graph.

Lists every crate that is compiled into the shipped programs (xrmc-desktop,
xrmc-android, xrmc-cli) as a normal (non-dev, non-build) dependency, on any
target, plus the non-Rust components that ship with or are loaded by the app.

    python3 scripts/third-party-notices.py            # writes THIRD_PARTY_NOTICES.md
    python3 scripts/third-party-notices.py --check    # exit 1 if the file is stale

Needs `cargo` (offline is fine once the crates are downloaded). Licenses come
from each crate's Cargo.toml `license` field.
"""

import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "THIRD_PARTY_NOTICES.md")
ROOTS = ["xrmc-desktop", "xrmc-android", "xrmc-cli"]

# Licenses that are fine to ship in an MIT app (and in Dunk XR hardware) without
# extra obligations beyond keeping the notices. Anything else is flagged.
PERMISSIVE = {
    "MIT", "MIT-0", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause",
    "BSD-3-Clause", "ISC", "Zlib", "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0",
    "Unlicense", "BSL-1.0", "0BSD", "OFL-1.1", "Ubuntu-font-1.0", "bzip2-1.0.6",
}

OTHER_COMPONENTS = """\
## Other components

| Component | License | How it is used |
|---|---|---|
| [scrcpy-server](https://github.com/Genymobile/scrcpy) v5.0.1 | Apache-2.0 | Bundled as a file in every package and pushed to each headset, where it captures and encodes the display. Unmodified upstream release. |
| [FFmpeg](https://ffmpeg.org) (libavcodec, libavutil and their dependencies) | LGPL-2.1-or-later | Loaded at runtime with `libloading`; never linked into the program. Windows packages bundle LGPL shared builds (DLLs) next to the program, which you can replace with your own compatible build. Linux uses the system libraries; macOS uses Homebrew's if installed. Source code: <https://ffmpeg.org/download.html>. For the exact sources of the DLLs in a given release, open an issue or email info@dunktechnologies.com. |
| [OpenH264](https://github.com/cisco/openh264) | BSD-2-Clause, © Cisco Systems | H.264 software decoder, compiled in through the `openh264` / `openh264-sys2` crates where enabled, or loaded at runtime from Cisco's binary. |
| Fonts embedded by egui (`epaint_default_fonts`) | Ubuntu-Light: Ubuntu Font Licence 1.0. Hack: MIT, plus the Bitstream Vera licence for the parts derived from it. Noto Emoji: OFL-1.1. emoji-icon-font (and its egui-icons subset): MIT. | Built into the program as the UI typefaces. The licence texts are in the crate's `fonts/` folder. |
| [Android GameActivity](https://developer.android.com/games/agdk/game-activity) (`androidx.games:games-activity`) | Apache-2.0 | Android app only: the activity the Rust app runs on. |
| AndroidX / Android NDK runtime (`libc++_shared.so`) | Apache-2.0; Apache-2.0 WITH LLVM-exception | Android app only. |
| [Electron](https://www.electronjs.org) and npm packages (`ws`, `qrcode`, …) | MIT | Legacy Electron app in `src/` only; not part of the native release. |

### scrcpy notice

```
Copyright (C) 2018 Genymobile
Copyright (C) 2018-2026 Romain Vimont

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

### OpenH264 notice

```
Copyright (c) 2013, Cisco Systems
All rights reserved.

Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice, this
  list of conditions and the following disclaimer in the documentation and/or
  other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### FFmpeg notice

This software uses libraries from the FFmpeg project under the LGPLv2.1 or
later. FFmpeg is a trademark of Fabrice Bellard, originator of the FFmpeg
project. XR MultiCast only loads FFmpeg's shared libraries at runtime; you may
replace them with any compatible build (libavcodec 59–62).
"""


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def shipped_packages(meta):
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    members = set(meta["workspace_members"])
    start = [p["id"] for p in meta["packages"] if p["name"] in ROOTS and p["id"] in members]
    missing = set(ROOTS) - {pkgs[i]["name"] for i in start}
    if missing:
        sys.exit(f"workspace packages not found: {', '.join(sorted(missing))}")
    seen, stack = set(start), list(start)
    while stack:
        for dep in nodes[stack.pop()]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])
    return [pkgs[i] for i in seen if i not in members]


def license_of(p):
    lic = p.get("license")
    if lic:
        return re.sub(r"\s*/\s*", " OR ", lic.strip())
    if p.get("license_file"):
        return "see " + os.path.basename(p["license_file"])
    return "UNKNOWN"


def permissive(expr):
    """True if some OR-alternative consists only of permissive licenses."""
    expr = expr.replace("(", " ").replace(")", " ")
    for alt in re.split(r"\s+OR\s+", expr):
        parts = [x.strip() for x in re.split(r"\s+AND\s+", alt)]
        if all(x in PERMISSIVE for x in parts):
            return True
    return False


def render(meta):
    pkgs = sorted(shipped_packages(meta), key=lambda p: (p["name"].lower(), p["version"]))
    flagged = [p for p in pkgs if not permissive(license_of(p))]
    counts = {}
    for p in pkgs:
        counts[license_of(p)] = counts.get(license_of(p), 0) + 1

    lines = [
        "# Third-party notices",
        "",
        "XR MultiCast is MIT-licensed (see [LICENSE](LICENSE)). It is built on the open-source",
        "components below; thank you to everyone who maintains them.",
        "",
        "<!-- Generated by scripts/third-party-notices.py; do not edit by hand. -->",
        "",
        OTHER_COMPONENTS,
        "## Rust crates",
        "",
        f"{len(pkgs)} crates are compiled into the shipped programs (`xr-multicast`, the Android app",
        "and the `xrmc` command-line tool) on at least one platform. Development and build-only",
        "dependencies are not listed. Each crate's licence text is in its source package on",
        "[crates.io](https://crates.io); where a crate offers a choice (`A OR B`), XR MultiCast uses",
        "it under the permissive option.",
        "",
        "### Licence summary",
        "",
        "| Licence | Crates |",
        "|---|---|",
    ]
    for lic, n in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])):
        lines.append(f"| {lic} | {n} |")
    lines += ["", "### Crates", "", "| Crate | Version | Licence | Source |", "|---|---|---|---|"]
    for p in pkgs:
        src = p.get("repository") or f"https://crates.io/crates/{p['name']}"
        lines.append(f"| {p['name']} | {p['version']} | {license_of(p)} | <{src}> |")
    lines.append("")
    return "\n".join(lines), flagged


def main():
    text, flagged = render(metadata())
    for p in flagged:
        print(f"review licence: {p['name']} {p['version']}: {license_of(p)}", file=sys.stderr)
    if "--check" in sys.argv:
        current = open(OUT, encoding="utf-8").read() if os.path.exists(OUT) else ""
        if current != text:
            sys.exit("THIRD_PARTY_NOTICES.md is stale: run python3 scripts/third-party-notices.py")
        return
    with open(OUT, "w", encoding="utf-8") as f:
        f.write(text)
    print(f"wrote {os.path.relpath(OUT, ROOT)}", file=sys.stderr)


if __name__ == "__main__":
    main()
