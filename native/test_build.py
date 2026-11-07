import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import tarfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("native_build", Path(__file__).with_name("build.py"))
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


class BuildVersionTests(unittest.TestCase):
    def test_discovery_uses_highest_published_stable_numeric_tag(self):
        releases = [
            {"tagName": "v1.0.0.9", "isDraft": False, "isPrerelease": False},
            {"tagName": "v1.0.0.77", "isDraft": False, "isPrerelease": False},
            {"tagName": "v2.0.0", "isDraft": True, "isPrerelease": False},
            {"tagName": "v3.0.0", "isDraft": False, "isPrerelease": True},
            {"tagName": "v4.0.0-beta", "isDraft": False, "isPrerelease": False},
            {"tagName": "v5.0.0"},
        ]
        self.assertEqual(BUILD.latest_stable(releases), "1.0.0.77")

    def test_missing_release_never_falls_back_to_package_version(self):
        with self.assertRaises(ValueError):
            BUILD.latest_stable([])

    def test_invalid_stamp_is_rejected(self):
        for value in ["", "v1.0.0", "1.0", "1.0.0.0.1", "1.0.0\n", "1.0.65536", "1.0.0-beta"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                BUILD.validate_version(value)

    def test_offline_build_requires_explicit_version_without_network(self):
        with patch.object(BUILD.subprocess, "run", side_effect=AssertionError("No subprocess expected")):
            with self.assertRaises(ValueError):
                BUILD.resolve_version(None, True)
            self.assertEqual(BUILD.resolve_version("1.0.0.77", True), "1.0.0.77")

    def test_sidecar_matches_artifact_and_existing_updater_format(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "kurisu-native"
            binary.write_bytes(b"abc")
            BUILD.write_sidecar(binary)
            self.assertEqual(
                binary.with_name("kurisu-native.sha256").read_text(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  kurisu-native\n",
            )


class PackagingTests(unittest.TestCase):
    def test_windows_release_embeds_runtime_and_stages_the_target_binary(self):
        for flags in ({"RUSTFLAGS": "-C opt-level=2"},
                      {"CARGO_ENCODED_RUSTFLAGS": "-C\x1fopt-level=2"},
                      {"CARGO_ENCODED_RUSTFLAGS": ""}):
            with self.subTest(flags=flags), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "PROFILE_TRANSFER.md").write_text("Transfer guide")

                def build(command, **options):
                    if command[0] == "cargo":
                        target = command[command.index("--target") + 1]
                        self.assertEqual(target, "x86_64-pc-windows-msvc")
                        environment = options["env"]
                        key = next(iter(flags))
                        self.assertTrue(environment[key].startswith(flags[key]))
                        self.assertTrue(environment[key].endswith("target-feature=+crt-static"))
                        compiled = root / "target" / target / "release/kurisu-native.exe"
                        compiled.parent.mkdir(parents=True)
                        compiled.write_bytes(b"standalone Windows executable")
                    else:
                        self.assertEqual(Path(command[0]).read_bytes(), b"standalone Windows executable")
                    return BUILD.subprocess.CompletedProcess(command, 0, stdout="kurisu-native 1.1.0\n")

                with patch.object(BUILD, "ROOT", root), patch.object(BUILD.sys, "platform", "win32"), \
                        patch.dict(os.environ, flags, clear=True), \
                        patch.object(BUILD.subprocess, "run", side_effect=build), \
                        patch.object(BUILD, "write_notices"):
                    self.assertEqual(BUILD.main(["--offline", "--build-version", "1.1.0"]), 0)
                staged = root / "target/release/kurisu-native.exe"
                self.assertEqual(staged.read_bytes(), b"standalone Windows executable")
                self.assertTrue(staged.with_name(staged.name + ".sha256").is_file())

    def test_windows_artifact_cannot_match_published_legacy_updater(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "kurisu-native.exe"
            binary.write_bytes(b"native")
            def compile_installer(command, **_):
                output = next(value.removeprefix("/DOUTPUT=") for value in command if value.startswith("/DOUTPUT="))
                Path(output).write_bytes(b"installer")
            with patch.object(BUILD, "ROOT", root), patch.object(BUILD.shutil, "which", return_value="makensis"), \
                    patch.object(BUILD.subprocess, "run", side_effect=compile_installer) as run:
                BUILD.package_windows("1.1.0", binary)
            installer = root / "target/release/bundle/Kurisu-Native_1.1.0_x64-install.exe"
            self.assertTrue(installer.is_file())
            self.assertTrue(installer.with_name(installer.name + ".sha256").is_file())
            self.assertFalse(installer.name.endswith("-setup.exe"))
            self.assertIn("/DWINDOWS_VERSION=1.1.0.0", run.call_args.args[0])

    def test_linux_fresh_bundle_has_normal_launcher_notices_icon_and_no_python(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "packaging").mkdir()
            binary = root / "kurisu-native"
            for name in ("kurisu-native", "LICENSE", "THIRD_PARTY_NOTICES.txt", "PROFILE_TRANSFER.md"):
                (root / name).write_text(name)
            binary.chmod(0o755)
            for name in ("kurisu.png", "kurisu.desktop"):
                (root / "packaging" / name).write_text(name)
            with patch.object(BUILD, "ROOT", root):
                path = BUILD.package_linux("1.1.0", binary)
            with tarfile.open(path) as archive:
                self.assertEqual(set(archive.getnames()), {
                    "Kurisu/kurisu", "Kurisu/LICENSE", "Kurisu/THIRD_PARTY_NOTICES.txt",
                    "Kurisu/PROFILE_TRANSFER.md", "Kurisu/kurisu.png", "Kurisu/kurisu.desktop",
                })
                self.assertEqual(archive.extractfile("Kurisu/kurisu").read(), b"kurisu-native")
                self.assertTrue(archive.getmember("Kurisu/kurisu").mode & 0o111)
            self.assertTrue(path.with_name(path.name + ".sha256").exists())

    def test_installer_retains_normal_identity_and_never_deletes_profile_trees(self):
        script = (BUILD.ROOT / "packaging/windows.nsi").read_text()
        for contract in ['Name "Kurisu"', 'InstallDir "$LOCALAPPDATA\\Kurisu"',
                         'Software\\CateDesu\\Kurisu', 'Uninstall\\Kurisu"',
                         'File /oname=kurisu.exe', 'Call EnsureStopped', 'Call un.EnsureStopped',
                         'CreateShortcut "$SMPROGRAMS\\Kurisu.lnk"', 'MUI_FINISHPAGE_RUN "$INSTDIR\\kurisu.exe"',
                         'Process32FirstW', 'kurisu-native.exe', 'THIRD_PARTY_NOTICES.txt', 'kurisu.ico']:
            self.assertIn(contract, script)
        self.assertNotIn('RMDir /r', script)
        self.assertNotIn('TerminateProcess', script)
        self.assertNotIn('taskkill', script)
        self.assertNotIn('File "..\\migrate.py"', script)
        self.assertNotIn('Delete "$LOCALAPPDATA', script)
        self.assertNotIn('Delete "$APPDATA', script)
        self.assertIn('$CandidateDir == "$LOCALAPPDATA\\Programs\\Kurisu Native"', script)


class DependencyNoticeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.supplements = self.root / "supplements"
        self.supplements.mkdir()
        (self.supplements / "index.json").write_text("{}", encoding="utf-8")

    def package(self, name, text="Permission notice\n"):
        root = self.root / name
        root.mkdir()
        if text is not None:
            (root / "LICENSE").write_text(text, encoding="utf-8")
        return {"id": name, "name": name, "version": "1.0.0", "license": "MIT",
                "manifest_path": str(root / "Cargo.toml"), "repository": "https://example.org/" + name,
                "homepage": None, "authors": ["Example author"], "license_file": None}

    @staticmethod
    def metadata(packages, dependencies):
        return {"packages": packages, "resolve": {"root": packages[0]["id"], "nodes": [
            {"id": package["id"], "deps": [
                {"pkg": name, "dep_kinds": [{"kind": kind}]}
                for name, kind in dependencies.get(package["id"], [])
            ]} for package in packages
        ]}}

    def test_transitive_build_dependencies_included_and_dev_only_excluded(self):
        packages = [self.package(name) for name in ("app", "normal", "build", "transitive", "dev")]
        metadata = self.metadata(packages, {
            "app": [("normal", None), ("build", "build"), ("dev", "dev")],
            "normal": [("transitive", None)], "transitive": [("normal", None)],
        })
        self.assertEqual([p["name"] for p in BUILD.dependency_packages(metadata)],
                         ["app", "build", "normal", "transitive"])
        notices = BUILD.collect_notices(metadata, self.supplements)
        self.assertNotIn("dev@1.0.0", notices)
        self.assertIn("transitive@1.0.0: LICENSE", notices)
        self.assertIn("build@1.0.0: LICENSE", notices)
        self.assertEqual(notices.count("Permission notice\n"), 1)

    def test_missing_text_stops_packaging(self):
        metadata = self.metadata([self.package("missing", None)], {})
        (self.root / "missing/AUTHORS").write_text("Example author\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "Missing license text for missing@1.0.0"):
            BUILD.collect_notices(metadata, self.supplements)

    def test_supplement_keeps_provenance_and_rejects_changed_declaration(self):
        metadata = self.metadata([self.package("missing", None)], {})
        (self.supplements / "permission.txt").write_text("Preserved upstream text\n", encoding="utf-8")
        (self.supplements / "index.json").write_text(json.dumps({"missing@1.0.0": {
            "license": "MIT", "provenance": "Published revision abc", "files": [
                {"path": "permission.txt", "name": "LICENSE", "source": "https://example.org/abc/LICENSE"}
            ]}}), encoding="utf-8")
        notices = BUILD.collect_notices(metadata, self.supplements)
        self.assertIn("Published revision abc", notices)
        self.assertIn("Preserved upstream text\n", notices)
        self.assertIn("Source: https://example.org/abc/LICENSE", notices)
        metadata["packages"][0]["license"] = "BSD-3-Clause"
        with self.assertRaisesRegex(ValueError, "License declaration changed"):
            BUILD.collect_notices(metadata, self.supplements)

    def test_nested_font_license_is_retained(self):
        package = self.package("with-font")
        font = self.root / "with-font/assets/font"
        font.mkdir(parents=True)
        (font / "license.txt").write_text("Font permission text\n", encoding="utf-8")
        notices = BUILD.collect_notices(self.metadata([package], {}), self.supplements)
        self.assertIn("with-font@1.0.0: assets/font/license.txt", notices)
        self.assertIn("Font permission text\n", notices)

    def test_metadata_preserves_unicode_under_windows_code_page(self):
        package = self.package("unicode", "Permission for Māori authors\n")
        package["authors"] = ["Māori author"]
        metadata = self.metadata([package], {})
        original_run = BUILD.subprocess.run
        original_notices = BUILD.collect_notices

        def tool_output(command, **options):
            if command[0] == "rustc":
                output = b"host: x86_64-pc-windows-msvc\n"
            else:
                self.assertEqual(command[:2], ["cargo", "metadata"])
                output = json.dumps(metadata, ensure_ascii=False).encode("utf-8")
            return original_run(
                [sys.executable, "-c", f"import sys; sys.stdout.buffer.write({output!r})"],
                **options,
            )

        with patch.object(BUILD.subprocess, "run", side_effect=tool_output), \
                patch.object(BUILD.subprocess, "_text_encoding", return_value="cp1252"), \
                patch.object(BUILD, "collect_notices", side_effect=lambda value: original_notices(value, self.supplements)):
            BUILD.write_notices(self.root, offline=True)
        notices = (self.root / "THIRD_PARTY_NOTICES.txt").read_text(encoding="utf-8")
        self.assertIn("Authors: Māori author", notices)
        self.assertIn("Permission for Māori authors", notices)


if __name__ == "__main__":
    unittest.main()
