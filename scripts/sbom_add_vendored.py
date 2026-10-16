#!/usr/bin/env python3
"""Add the vendored packages that Syft cannot see to the release SBOMs.

Syft builds the SBOMs from Cargo.lock, which lists only Rust crates. The
binary also links the vendored Zig terminal engine (libghostty-vt) and the
C/C++/Zig libraries it compiles in, and the Windows package ships Microsoft's
ConPTY. Their versions are read from the files that pin them, so the SBOM
follows any vendor update without a second place to edit.

Usage: sbom_add_vendored.py <repo-root> <sbom.cdx.json> <sbom.spdx.json>
"""

import json
import re
import sys
from pathlib import Path


def zon(root: Path, rel: str, pattern: str) -> str:
    text = (root / rel).read_text(encoding="utf-8")
    match = re.search(pattern, text)
    if not match:
        sys.exit(f"sbom_add_vendored: no match for {pattern!r} in {rel}")
    return match.group(1)


def vendored(root: Path) -> list[dict]:
    vt = json.loads((root / "vendor/libghostty-vt.vendor.json").read_text(encoding="utf-8"))
    conpty = json.loads((root / "packaging/windows/conpty.json").read_text(encoding="utf-8"))["package"]
    main_zon = "vendor/libghostty-vt/build.zig.zon"
    uucode = zon(root, main_zon, r"uucode-([0-9a-f]{40})\.tar")
    highway = zon(root, "vendor/libghostty-vt/pkg/highway/build.zig.zon", r"highway-([0-9a-f]{40})\.tar")
    wuffs = zon(root, "vendor/libghostty-vt/pkg/wuffs/build.zig.zon", r"wuffs-([0-9a-f]{40})\.tar")
    simdutf = zon(root, "vendor/libghostty-vt/pkg/simdutf/build.zig.zon", r'\.version\s*=\s*"([^"]+)"')
    highway_version = zon(root, "vendor/libghostty-vt/pkg/highway/build.zig.zon", r'\.version\s*=\s*"([^"]+)"')
    return [
        {
            "name": "libghostty-vt",
            "version": vt["source_commit"],
            "purl": f"pkg:github/ghostty-org/ghostty@{vt['source_commit']}",
            "license": "MIT",
            "note": "vendored terminal engine (vendor/libghostty-vt)",
        },
        {
            "name": "uucode",
            "version": uucode,
            "purl": f"pkg:github/jacobsandlund/uucode@{uucode}",
            "license": "MIT",
            "note": "compiled into libghostty-vt",
        },
        {
            "name": "simdutf",
            "version": simdutf,
            "purl": f"pkg:github/simdutf/simdutf@v{simdutf}",
            "license": "Apache-2.0 OR MIT",
            "note": "compiled into libghostty-vt",
        },
        {
            "name": "highway",
            "version": highway_version,
            "purl": f"pkg:github/google/highway@{highway}",
            "license": "Apache-2.0 OR BSD-3-Clause",
            "note": "compiled into libghostty-vt",
        },
        {
            "name": "wuffs",
            "version": wuffs,
            "purl": f"pkg:github/google/wuffs@{wuffs}",
            "license": "Apache-2.0 OR MIT",
            "note": "compiled into libghostty-vt",
        },
        {
            "name": conpty["id"],
            "version": conpty["version"],
            "purl": f"pkg:nuget/{conpty['id']}@{conpty['version']}",
            "license": conpty["license"],
            "note": "bundled in the Windows package (conpty.dll, OpenConsole.exe)",
        },
    ]


def add_cyclonedx(path: Path, packages: list[dict]) -> None:
    bom = json.loads(path.read_text(encoding="utf-8"))
    components = bom.setdefault("components", [])
    for pkg in packages:
        components.append({
            "bom-ref": pkg["purl"],
            "type": "library",
            "name": pkg["name"],
            "version": pkg["version"],
            "purl": pkg["purl"],
            "licenses": [{"expression": pkg["license"]}],
            "properties": [{"name": "kelpie:source", "value": pkg["note"]}],
        })
    path.write_text(json.dumps(bom, indent=2) + "\n", encoding="utf-8")


def add_spdx(path: Path, packages: list[dict]) -> None:
    doc = json.loads(path.read_text(encoding="utf-8"))
    described = [
        rel["relatedSpdxElement"]
        for rel in doc.get("relationships", [])
        if rel.get("spdxElementId") == "SPDXRef-DOCUMENT" and rel.get("relationshipType") == "DESCRIBES"
    ]
    for pkg in packages:
        spdx_id = "SPDXRef-Package-vendored-" + re.sub(r"[^A-Za-z0-9.-]", "-", pkg["name"])
        doc.setdefault("packages", []).append({
            "SPDXID": spdx_id,
            "name": pkg["name"],
            "versionInfo": pkg["version"],
            "downloadLocation": "NOASSERTION",
            "filesAnalyzed": False,
            "licenseConcluded": "NOASSERTION",
            "licenseDeclared": pkg["license"],
            "copyrightText": "NOASSERTION",
            "comment": pkg["note"],
            "externalRefs": [{
                "referenceCategory": "PACKAGE-MANAGER",
                "referenceType": "purl",
                "referenceLocator": pkg["purl"],
            }],
        })
        for root in described:
            doc.setdefault("relationships", []).append({
                "spdxElementId": root,
                "relatedSpdxElement": spdx_id,
                "relationshipType": "CONTAINS",
            })
    path.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    root, cdx, spdx = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    packages = vendored(root)
    add_cyclonedx(cdx, packages)
    add_spdx(spdx, packages)
    for pkg in packages:
        print(f"added {pkg['name']} {pkg['version']}")


if __name__ == "__main__":
    main()
