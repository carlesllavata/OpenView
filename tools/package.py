# SPDX-License-Identifier: GPL-3.0-or-later
"""Builds OpenView and packs it for a release.

    python -I tools/package.py            build, test and write dist/
    python -I tools/package.py --no-test  skip the tests

Writes dist/openview-<version>-windows-x64.zip (OpenView.exe, README.md, LICENSE, CHANGELOG.md and
THIRD-PARTY-LICENSES.txt) and dist/SHA256SUMS.txt. Refuses to build when the version in Cargo.toml
is not the one the README and the changelog talk about, or when a library has a licence that
cannot be distributed under GPL-3.0-or-later.

Only the standard library is used, so any Python 3.9+ works.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
TARGET = "x86_64-pc-windows-msvc"

# Licences of libraries that may be linked into a GPL-3.0-or-later program.
COMPATIBLE = {
    "0BSD", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause", "BSL-1.0",
    "CC0-1.0", "CDLA-Permissive-2.0", "ISC", "LGPL-2.1-or-later", "LGPL-3.0-or-later", "MIT", "MIT-0",
    "MPL-2.0", "OFL-1.1", "Ubuntu-font-1.0", "Unicode-3.0", "Unicode-DFS-2016", "Unlicense", "Zlib",
    "GPL-3.0-or-later",
}
LICENCE_FILES = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE|COPYRIGHT)", re.IGNORECASE)


def fail(message):
    sys.exit(f"package.py: {message}")


def cargo():
    found = shutil.which("cargo")
    if found:
        return found
    home = Path(os.environ.get("USERPROFILE", "")) / ".cargo" / "bin" / "cargo.exe"
    if home.exists():
        return str(home)
    fail("cargo was not found")


def run(*args):
    print(">", " ".join(str(a) for a in args), flush=True)
    result = subprocess.run([str(a) for a in args], cwd=ROOT)
    if result.returncode != 0:
        fail(f"{args[1]} failed")


def version():
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    found = re.search(r'^version = "([^"]+)"', text, re.MULTILINE)
    if not found:
        fail("no version in Cargo.toml")
    return found.group(1)


def check_documents(ver):
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    if not re.search(rf"^\*\*Version {re.escape(ver)}\b", readme, re.MULTILINE):
        fail(f"README.md does not say '**Version {ver}' on its status line")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    if f"## {ver}" not in changelog and "## In development" not in changelog:
        fail(f"CHANGELOG.md has no section for {ver}")


def licence_terms(expression):
    """The alternatives of a licence expression: a list (AND) of sets (OR)."""
    expression = expression.replace("/", " OR ").replace("(", " ").replace(")", " ")
    return [{term.strip() for term in part.split(" OR ") if term.strip()} for part in expression.split(" AND ")]


def third_party(ver):
    """Checks every library's licence and gathers their licence texts in one file."""
    out = subprocess.run(
        [cargo(), "metadata", "--format-version", "1", "--filter-platform", TARGET, "--locked"],
        cwd=ROOT, capture_output=True, text=True, encoding="utf-8",
    )
    if out.returncode != 0:
        fail("cargo metadata failed: " + out.stderr.strip())
    meta = json.loads(out.stdout)
    used = {node["id"] for node in meta["resolve"]["nodes"]}
    # Libraries only needed to build or to test are not in the executable: follow normal
    # dependencies only, starting from OpenView itself.
    root = meta["resolve"]["root"]
    linked, queue = set(), [root]
    by_id = {node["id"]: node for node in meta["resolve"]["nodes"]}
    while queue:
        current = queue.pop()
        if current in linked:
            continue
        linked.add(current)
        for dep in by_id[current]["deps"]:
            if any(k.get("kind") is None for k in dep["dep_kinds"]):
                queue.append(dep["pkg"])
    packages = sorted((p for p in meta["packages"] if p["id"] in linked and p["id"] in used and p["id"] != root),
                      key=lambda p: (p["name"].lower(), p["version"]))
    lines = [
        f"OpenView {ver} contains the following libraries. Each is listed with its licence and,",
        "below, the licence texts that come with it.",
        "",
    ]
    problems = []
    for package in packages:
        licence = package.get("license") or "(see its licence file)"
        lines.append(f"  {package['name']} {package['version']}  -  {licence}")
        if package.get("license"):
            for alternatives in licence_terms(package["license"]):
                if not alternatives & COMPATIBLE:
                    problems.append(f"{package['name']} {package['version']}: {package['license']}")
    if problems:
        fail("licences that need a look before distributing:\n  " + "\n  ".join(problems))
    for package in packages:
        folder = Path(package["manifest_path"]).parent
        texts = sorted(f for f in folder.iterdir() if f.is_file() and LICENCE_FILES.match(f.name))
        if not texts:
            continue
        lines += ["", "=" * 100, f"{package['name']} {package['version']}", "=" * 100]
        for text in texts:
            lines += ["", f"--- {text.name} ---", "", text.read_text(encoding="utf-8", errors="replace").rstrip()]
    print(f"  {len(packages)} libraries, all with licences compatible with GPL-3.0-or-later")
    return "\n".join(lines) + "\n"


def target_dir():
    out = subprocess.run([cargo(), "metadata", "--format-version", "1", "--no-deps"], cwd=ROOT,
                         capture_output=True, text=True, encoding="utf-8")
    return Path(json.loads(out.stdout)["target_directory"])


def main():
    ver = version()
    check_documents(ver)
    notices = third_party(ver)
    if "--no-test" not in sys.argv:
        run(cargo(), "test", "--locked")
    run(cargo(), "build", "--release", "--locked")
    exe = target_dir() / "release" / "OpenView.exe"
    if not exe.exists():
        fail(f"{exe} was not built")
    said = subprocess.run([str(exe), "--version"], capture_output=True, text=True).stdout.strip()
    if said != f"OpenView {ver}":
        fail(f"the executable says '{said}', not 'OpenView {ver}'")

    DIST.mkdir(exist_ok=True)
    name = f"openview-{ver}-windows-x64.zip"
    archive = DIST / name
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        z.write(exe, "OpenView.exe")
        for document in ("README.md", "LICENSE", "CHANGELOG.md"):
            z.write(ROOT / document, document)
        z.writestr("THIRD-PARTY-LICENSES.txt", notices)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (DIST / "SHA256SUMS.txt").write_text(f"{digest}  {name}\n", encoding="utf-8", newline="\n")
    print(f"wrote {archive} ({archive.stat().st_size / 1e6:.1f} MB)\n  sha256 {digest}")


if __name__ == "__main__":
    main()
