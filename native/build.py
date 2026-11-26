#!/usr/bin/env python3
"""Build Kurisu without installing or publishing it."""

import argparse
from contextlib import contextmanager
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile

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


def build_target():
    if sys.platform == "win32":
        return "x86_64-pc-windows-msvc"
    rustc = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, encoding="utf-8")
    host = next(line.removeprefix("host: ") for line in rustc.stdout.splitlines() if line.startswith("host: "))
    if sys.platform == "linux" and host not in ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"):
        raise ValueError("Linux release packaging requires an x86_64 Linux host")
    return host


def write_notices(destination, offline, target=None):
    target = target or build_target()
    command = ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target,
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
    items = [
        item
        for entry in releases
        for item in (entry if isinstance(entry, list) else [entry])
    ]
    versions = []
    for release in items:
        draft = release.get("draft") if "draft" in release else release.get("isDraft")
        prerelease = release.get("prerelease") if "prerelease" in release else release.get("isPrerelease")
        if draft is not False or prerelease is not False:
            continue
        tag = release.get("tag_name") if "tag_name" in release else release.get("tagName", "")
        if not isinstance(tag, str):
            continue
        try:
            versions.append(validate_version(tag.removeprefix("v")))
        except ValueError:
            continue
    if not versions:
        raise ValueError("No published stable numeric release was found")
    return max(
        versions,
        key=lambda value: (parts := tuple(map(int, value.split(".")))) + (0,) * (4 - len(parts)),
    )


def resolve_version(explicit, offline):
    if explicit is not None:
        return validate_version(explicit)
    if offline:
        raise ValueError("Offline builds require --build-version or KURISU_BUILD_VERSION")
    repository = os.environ.get("KURISU_RELEASE_REPOSITORY") or REPOSITORY
    response = subprocess.run(
        ["gh", "api", "--paginate", "--slurp", f"repos/{repository}/releases?per_page=100"],
        check=True, capture_output=True, encoding="utf-8", timeout=60,
    )
    return latest_stable(json.loads(response.stdout))


def write_sidecar(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    sidecar = path.with_name(path.name + ".sha256")
    sidecar.write_text(f"{digest.hexdigest()}  {path.name}\n", encoding="utf-8")
    return sidecar


@contextmanager
def stage_artifact(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(dir=path.parent, prefix=".packaging-"))
    keep_temporary = False
    try:
        staged = temporary / path.name
        yield staged
        metadata = staged.lstat()
        if not stat.S_ISREG(metadata.st_mode) or not metadata.st_size:
            raise ValueError(f"Expected a nonempty artifact: {path.name}")
        sidecar = write_sidecar(staged)
        previous = temporary / "previous"
        try:
            existing = path.lstat()
        except FileNotFoundError:
            existing = None
        if existing is not None:
            if not stat.S_ISREG(existing.st_mode):
                raise ValueError(f"Expected a regular artifact: {path}")
            shutil.copy2(path, previous)
        staged.replace(path)
        try:
            sidecar.replace(path.with_name(sidecar.name))
        except OSError:
            try:
                if existing is None:
                    path.unlink()
                else:
                    previous.replace(path)
            except OSError as error:
                keep_temporary = True
                raise OSError(f"Could not restore {path}: {error}. Recovery files are in {temporary}") from error
            raise
    finally:
        if not keep_temporary:
            try:
                shutil.rmtree(temporary)
            except OSError as error:
                print(f"Could not clean packaging stage {temporary}: {error}", file=sys.stderr)


def package_windows(version, binary):
    compiler = shutil.which("makensis")
    if compiler is None:
        compiler_path = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "NSIS/makensis.exe"
        if compiler_path.is_file():
            compiler = str(compiler_path)
    if compiler is None:
        raise ValueError("NSIS 3 is required for --installer")
    bundle = ROOT / "target/release/bundle"
    installer = bundle / f"Kurisu-Native_{version}_x64-install.exe"
    windows_version = ".".join((version.split(".") + ["0"] * 4)[:4])
    with stage_artifact(installer) as staged:
        subprocess.run(
            [compiler, f"/DVERSION={version}", f"/DWINDOWS_VERSION={windows_version}",
             f"/DBINARY={binary}", f"/DNOTICES={binary.parent / 'THIRD_PARTY_NOTICES.txt'}",
             f"/DOUTPUT={staged}", str(ROOT / "packaging/windows.nsi")],
            check=True,
        )
    print(installer)


def package_linux(version, binary):
    bundle = ROOT / "target/release/bundle"
    archive = bundle / f"Kurisu_{version}_linux-x64.tar.gz"
    with (
        stage_artifact(archive) as staged,
        staged.open("wb") as compressed,
        gzip.GzipFile(filename="", mode="wb", fileobj=compressed, mtime=0) as stream,
        tarfile.open(fileobj=stream, mode="w") as output,
    ):
        for source, name in [
            (binary, "kurisu"),
            (binary.parent / "LICENSE", "LICENSE"),
            (binary.parent / "THIRD_PARTY_NOTICES.txt", "THIRD_PARTY_NOTICES.txt"),
            (ROOT / "packaging/kurisu.png", "kurisu.png"),
            (ROOT / "packaging/kurisu.desktop", "kurisu.desktop"),
        ]:
            entry = output.gettarinfo(source, arcname=f"Kurisu/{name}")
            if entry is None or not entry.isfile() or not entry.size:
                raise ValueError(f"Expected a nonempty bundle input: {source}")
            entry.mode = 0o755 if name == "kurisu" else 0o644
            entry.mtime = 0
            entry.uid = entry.gid = 0
            entry.uname = entry.gname = ""
            with source.open("rb") as content:
                output.addfile(entry, content)
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
        target = build_target()
        notice_dir = ROOT / "target/notice-input" / target
        notice_dir.mkdir(parents=True, exist_ok=True)
        write_notices(notice_dir, args.offline, target)
        command = ["cargo", "build", "--locked", "--release", "--target", target, "--manifest-path",
                   str(ROOT / "Cargo.toml"), "--target-dir", str(ROOT / "target")]
        if args.offline:
            command.append("--offline")
        environment = os.environ.copy()
        environment["KURISU_BUILD_VERSION"] = version
        environment["KURISU_BUNDLED_NOTICES"] = str(notice_dir / "THIRD_PARTY_NOTICES.txt")
        environment.setdefault("CARGO_BUILD_JOBS", "3")
        binary = ROOT / "target/release" / ("kurisu-native.exe" if sys.platform == "win32" else "kurisu-native")
        compiled = ROOT / "target" / target / "release" / binary.name
        if sys.platform == "win32":
            if "CARGO_ENCODED_RUSTFLAGS" in environment:
                environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(filter(None, [
                    environment["CARGO_ENCODED_RUSTFLAGS"], "-C", "target-feature=+crt-static",
                ]))
            else:
                environment["RUSTFLAGS"] = (
                    environment.get("RUSTFLAGS", "") + " -C target-feature=+crt-static"
                ).strip()
        subprocess.run(command, check=True, env=environment)
        with stage_artifact(binary) as staged:
            shutil.copy2(compiled, staged)
            actual = subprocess.run([str(staged), "--version"], check=True, capture_output=True, encoding="utf-8")
            if actual.stdout.strip() != f"kurisu-native {version}":
                raise ValueError("The built executable does not report the requested version")
        for name in ("LICENSE", "THIRD_PARTY_NOTICES.txt"):
            shutil.copyfile(notice_dir / name, binary.parent / name)
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
