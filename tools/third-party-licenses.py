#!/usr/bin/env python3
"""Writes THIRD-PARTY-LICENSES.txt: the license text of every crate linked into the binary."""
import json, os, subprocess, sys

SLINT = {"slint", "slint-macros", "i-slint-backend-selector", "i-slint-backend-winit", "i-slint-common",
         "i-slint-core", "i-slint-core-macros", "i-slint-renderer-femtovg", "i-slint-renderer-software"}

meta = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"]))
packages = {(p["name"], "v" + p["version"]): p for p in meta["packages"]}

# cargo tree, because metadata also lists optional crates this build never compiles.
tree = subprocess.check_output(
    ["cargo", "tree", "--locked", "-e", "normal", "--prefix", "none", "--target", "x86_64-unknown-linux-gnu"], text=True)
seen = {tuple(line.split()[:2]) for line in tree.splitlines() if line.strip()}
seen = {key for key in seen if key[0] != "tailgate"}

out, missing = [], []
for key in sorted(seen):
    pkg = packages[key]
    out.append("=" * 78 + f"\n{pkg['name']} {pkg['version']}\nLicense: {pkg.get('license') or '(see text)'}\n")
    if pkg["name"] in SLINT:
        out.append("Used under LicenseRef-Slint-Royalty-free-2.0. See LICENSES/ and NOTICE.\n\n")
        continue
    root = os.path.dirname(pkg["manifest_path"])
    names = sorted(n for n in os.listdir(root)
                   if n.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE"))
                   and os.path.isfile(os.path.join(root, n)))
    if not names:
        missing.append(pkg["name"])
        out.append("The crate ships no license file. Its declared license is the one named above.\n\n")
    for name in names:
        with open(os.path.join(root, name), errors="replace") as f:
            out.append(f"--- {name} ---\n{f.read().strip()}\n\n")

with open(sys.argv[1] if len(sys.argv) > 1 else "THIRD-PARTY-LICENSES.txt", "w") as f:
    f.write("Third-party licenses for the Rust crates linked into Tailgate.\n\n" + "".join(out))
print(f"{len(seen)} crates, {len(missing)} without a license file: {' '.join(missing)}")
