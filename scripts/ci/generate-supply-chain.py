from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            text=True,
        )
    )
    packages = sorted(
        metadata["packages"],
        key=lambda package: (package["name"], package["version"]),
    )
    workspace_members = set(metadata["workspace_members"])
    components = [
        {
            "type": "library",
            "name": package["name"],
            "version": package["version"],
            "licenses": (
                [{"expression": package["license"]}] if package["license"] else []
            ),
            "purl": f"pkg:cargo/{package['name']}@{package['version']}",
        }
        for package in packages
    ]
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "components": components,
    }
    licenses = [
        {
            "name": package["name"],
            "version": package["version"],
            "license": package["license"],
            "licenseFile": (
                Path(package["license_file"]).name if package["license_file"] else None
            ),
        }
        for package in packages
    ]
    build_metadata = {
        "schemaVersion": 1,
        "locked": True,
        "packages": [
            {
                "name": package["name"],
                "version": package["version"],
                "source": package["source"],
                "checksum": package.get("checksum"),
                "workspace": package["id"] in workspace_members,
            }
            for package in packages
        ],
    }
    arguments.output.mkdir(parents=True, exist_ok=True)
    (arguments.output / "sbom.cdx.json").write_text(
        json.dumps(bom, indent=2, sort_keys=True) + "\n"
    )
    (arguments.output / "licenses.json").write_text(
        json.dumps(licenses, indent=2, sort_keys=True) + "\n"
    )
    (arguments.output / "build-metadata.json").write_text(
        json.dumps(build_metadata, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
