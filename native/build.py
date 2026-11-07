#!/usr/bin/env python3
"""Build Kurisu without installing or publishing it."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile

ROOT = Path(__file__).resolve().parent
REPOSITORY = "CateDesu/Kurisu"


def dependency_packages(metadata):
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = [metadata["resolve"]["root"]]
    included = set()
    while pending:
        package_id = pending.pop()
        if package_id in included:
            continue
        included.add(package_id)
        pending.extend(
            dep["pkg"] for dep in nodes[package_id]["deps"]
            if any(kind["kind"] != "dev" for kind in dep["dep_kinds"])
        )
    return sorted(
        (package for package in metadata["packages"] if package["id"] in included),
        key=lambda package: (package["name"], package["version"]),
    )


def collect_notices(metadata, supplements=ROOT / "packaging/licenses"):
    index = json.loads((supplements / "index.json").read_text(encoding="utf-8"))
    sections = [
        "Kurisu dependency notices\n\n"
        "Includes normal and build dependencies from the locked Cargo graph for this target.\n"
        "Build tools and bundled optional source notices may also be listed.\n"
        "Identical texts appear once with their package and source attributions.\n\n"
        "Portions of this software are copyright The FreeType Project.\n"
        "All rights reserved. FreeType is used under the FreeType License.\n"
    ]
    texts = {}

    def include(label, content):
        if not content.strip():
            raise ValueError(f"Empty license text: {label}")
        texts.setdefault(content, []).append(label)

    for package in dependency_packages(metadata):
        key = f"{package['name']}@{package['version']}"
        root = Path(package["manifest_path"]).parent
        sections.append(
            f"{key}\nLicense: {package['license'] or 'See included license file'}\n"
            f"Source: {package['repository'] or package['homepage'] or 'Cargo package'}\n"
            f"Authors: {', '.join(package['authors'])}\n"
        )
        if root.resolve() in (ROOT.resolve(), (ROOT.parent / "core").resolve()):
            files = [ROOT.parent / "LICENSE"]
        else:
            files = sorted(
                path for path in root.rglob("*") if path.is_file()
                and path.name.lower().startswith(
                    ("license", "licence", "copying", "copyright", "notice", "unlicense", "authors")
                ) and path.suffix.lower() not in (".rs", ".py", ".toml", ".html", ".c", ".h")
            )
            if package["license_file"]:
                files.append(root / package["license_file"])
            if package["name"] == "freetype-sys":
                files.append(root / "freetype2/docs/FTL.TXT")
        for path in dict.fromkeys(files):
            label = path.relative_to(root).as_posix() if path.is_relative_to(root) else path.name
            include(f"{key}: {label}", path.read_text(encoding="utf-8"))
        extra = index.get(key)
        if extra:
            if extra["license"] != package["license"]:
                raise ValueError(f"License declaration changed for {key}")
            sections.append(extra["provenance"] + "\n")
            for file in extra["files"]:
                include(
                    f"{key}: {file['name']}\nSource: {file['source']}",
                    (supplements / file["path"]).read_text(encoding="utf-8"),
                )
        has_license = bool(package["license_file"]) or any(
            path.name.lower().startswith(("license", "licence", "copying", "unlicense")) for path in files
        )
        if not has_license and not extra:
            raise ValueError(f"Missing license text for {key}; add a sourced supplement")
    sections.append("Local dependency changes\n\n" + (ROOT / "vendor/README.md").read_text(encoding="utf-8"))
    for content, labels in texts.items():
        sections.append("=" * 72 + "\n" + "\n".join(labels) + "\n\n" + content)
    return "\n".join(sections)


def write_notices(destination, offline):
    rustc = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, encoding="utf-8")
    host = next(line.removeprefix("host: ") for line in rustc.stdout.splitlines() if line.startswith("host: "))
    command = ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", host,
               "--manifest-path", str(ROOT / "Cargo.toml")]
    if offline:
        command.append("--offline")
    metadata = subprocess.run(command, check=True, capture_output=True, encoding="utf-8")
    notices = collect_notices(json.loads(metadata.stdout))
    (destination / "THIRD_PARTY_NOTICES.txt").write_text(notices, encoding="utf-8")
    shutil.copyfile(ROOT.parent / "LICENSE", destination / "LICENSE")


def validate_version(value):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:\.[0-9]+)?", value):
        raise ValueError("Build version must contain three or four numeric components")
    if any(int(part) > 65535 for part in value.split(".")):
        raise ValueError("Build version components must fit Windows version fields")
    return value


def latest_stable(releases):
    versions = []
    for release in releases:
        if release.get("isDraft") is not False or release.get("isPrerelease") is not False:
            continue
        tag = release.get("tagName", "")
        if not isinstance(tag, str):
            continue
        try:
            versions.append(validate_version(tag.removeprefix("v")))
        except ValueError:
            continue
    if not versions:
        raise ValueError("No published stable numeric release was found")
    return max(versions, key=lambda value: tuple(map(int, value.split("."))))


def resolve_version(explicit, offline):
    if explicit is not None:
        return validate_version(explicit)
    if offline:
        raise ValueError("Offline builds require --build-version or KURISU_BUILD_VERSION")
    response = subprocess.run(
        ["gh", "release", "list", "-R", REPOSITORY, "--limit", "100", "--json",
         "tagName,isDraft,isPrerelease"],
        check=True, capture_output=True, encoding="utf-8", timeout=60,
    )
    return latest_stable(json.loads(response.stdout))


def write_sidecar(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    path.with_name(path.name + ".sha256").write_text(
        f"{digest.hexdigest()}  {path.name}\n", encoding="utf-8"
    )


def package_windows(version, binary):
    compiler = shutil.which("makensis")
    if compiler is None:
        compiler_path = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "NSIS/makensis.exe"
        if compiler_path.is_file():
            compiler = str(compiler_path)
    if compiler is None:
        raise ValueError("NSIS 3 is required for --installer")
    bundle = ROOT / "target/release/bundle"
    bundle.mkdir(parents=True, exist_ok=True)
    installer = bundle / f"Kurisu-Native_{version}_x64-install.exe"
    windows_version = ".".join((version.split(".") + ["0"] * 4)[:4])
    subprocess.run(
        [compiler, f"/DVERSION={version}", f"/DWINDOWS_VERSION={windows_version}",
         f"/DBINARY={binary}", f"/DNOTICES={binary.parent / 'THIRD_PARTY_NOTICES.txt'}",
         f"/DOUTPUT={installer}", str(ROOT / "packaging/windows.nsi")],
        check=True,
    )
    write_sidecar(installer)
    print(installer)


def package_linux(version, binary):
    bundle = ROOT / "target/release/bundle"
    bundle.mkdir(parents=True, exist_ok=True)
    archive = bundle / f"Kurisu_{version}_linux-x64.tar.gz"
    with tarfile.open(archive, "w:gz") as output:
        for source, name in [
            (binary, "kurisu"),
            (binary.parent / "LICENSE", "LICENSE"),
            (binary.parent / "THIRD_PARTY_NOTICES.txt", "THIRD_PARTY_NOTICES.txt"),
            (binary.parent / "PROFILE_TRANSFER.md", "PROFILE_TRANSFER.md"),
            (ROOT / "packaging/kurisu.png", "kurisu.png"),
            (ROOT / "packaging/kurisu.desktop", "kurisu.desktop"),
        ]:
            entry = output.gettarinfo(source, arcname=f"Kurisu/{name}")
            entry.mode = 0o755 if name == "kurisu" else 0o644
            with source.open("rb") as content:
                output.addfile(entry, content)
    write_sidecar(archive)
    return archive


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-version", default=os.environ.get("KURISU_BUILD_VERSION") or None,
                        help="Explicit published release version, otherwise discover with gh")
    parser.add_argument("--offline", action="store_true", help="Use only cached Cargo dependencies")
    parser.add_argument("--print-version", action="store_true", help="Resolve the version without building")
    parser.add_argument("--installer", action="store_true", help="Also build the Windows NSIS installer")
    args = parser.parse_args(argv)
    try:
        if args.installer and sys.platform != "win32":
            raise ValueError("Build the Windows installer on Windows with the MSVC toolchain")
        version = resolve_version(args.build_version, args.offline)
        if args.print_version:
            print(version)
            return 0
        command = ["cargo", "build", "--locked", "--release", "--manifest-path",
                   str(ROOT / "Cargo.toml"), "--target-dir", str(ROOT / "target")]
        if args.offline:
            command.append("--offline")
        environment = os.environ.copy()
        environment["KURISU_BUILD_VERSION"] = version
        environment.setdefault("CARGO_BUILD_JOBS", "3")
        subprocess.run(command, check=True, env=environment)
        binary = ROOT / "target/release" / ("kurisu-native.exe" if sys.platform == "win32" else "kurisu-native")
        actual = subprocess.run([str(binary), "--version"], check=True, capture_output=True, encoding="utf-8")
        if actual.stdout.strip() != f"kurisu-native {version}":
            raise ValueError("The built executable does not report the requested version")
        write_sidecar(binary)
        write_notices(binary.parent, args.offline)
        shutil.copyfile(ROOT / "PROFILE_TRANSFER.md", binary.parent / "PROFILE_TRANSFER.md")
        print(binary)
        if args.installer:
            package_windows(version, binary)
        elif sys.platform == "linux":
            print(package_linux(version, binary))
        return 0
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"Native build failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
