import importlib.util
import json
import os
import select
import shutil
from pathlib import Path
import sys
import tempfile
import tarfile
import textwrap
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

                def notices(destination, offline, target=None):
                    self.assertEqual(target, "x86_64-pc-windows-msvc")
                    for name in ("LICENSE", "THIRD_PARTY_NOTICES.txt"):
                        (destination / name).write_text(name)

                with patch.object(BUILD, "ROOT", root), patch.object(BUILD.sys, "platform", "win32"), \
                        patch.dict(os.environ, flags, clear=True), \
                        patch.object(BUILD.subprocess, "run", side_effect=build), \
                        patch.object(BUILD, "write_notices", side_effect=notices):
                    self.assertEqual(BUILD.main(["--offline", "--build-version", "1.1.0"]), 0)
                staged = root / "target/release/kurisu-native.exe"
                self.assertEqual(staged.read_bytes(), b"standalone Windows executable")
                self.assertTrue(staged.with_name(staged.name + ".sha256").is_file())

    @unittest.skipUnless(sys.platform == "linux", "Linux release artifact")
    def test_linux_build_overrides_configured_target_and_embeds_current_notices(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "native"
            (root / "src").mkdir(parents=True)
            (root / "packaging").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname="kurisu-native"\nversion="1.2.3"\nedition="2024"\n')
            (root / "PROFILE_TRANSFER.md").write_text("Fixture transfer guide")
            for name in ("kurisu.png", "kurisu.desktop"):
                (root / "packaging" / name).write_text("fixture")
            environment = os.environ.copy()
            for name in ("CARGO_BUILD_TARGET", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "KURISU_BUNDLED_NOTICES"):
                environment.pop(name, None)
            target = BUILD.build_target()
            source = root / "src/main.rs"
            source.write_text('fn main() { if std::env::args().any(|s|s=="--version") { println!("kurisu-native 1.2.3"); } else { println!("old"); std::thread::sleep(std::time::Duration::from_secs(60)); } }')
            BUILD.subprocess.run(["cargo", "build", "--offline", "--release", "--target", target,
                                  "--manifest-path", str(root / "Cargo.toml"), "--target-dir", str(root / "target")],
                                 check=True, cwd=root, env=environment, capture_output=True, timeout=60)
            binary = root / "target/release/kurisu-native"
            binary.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / "target" / target / "release/kurisu-native", binary)
            shutil.copyfile(BUILD.ROOT / "build.rs", root / "build.rs")
            shutil.copyfile(BUILD.ROOT / "src/notices.rs", root / "src/notices.rs")
            license_text = (BUILD.ROOT.parent / "LICENSE").read_text()
            (root.parent / "LICENSE").write_text(license_text)
            shutil.copyfile(BUILD.ROOT / "src/main.rs", source)
            # Exercise the real entry point without starting GPUI or a user's profile.
            (root / "src/lib.rs").write_text(textwrap.dedent('''\
                pub mod notices;

                pub fn run(profile: Option<std::path::PathBuf>) -> Result<(), String> {
                    let profile = profile.ok_or("test requires an explicit profile")?;
                    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
                    let directory = executable.parent().unwrap();
                    let installed = format!("{}:{}",
                        directory.join("kurisu.LICENSE").is_file(),
                        directory.join("kurisu.THIRD_PARTY_NOTICES.txt").is_file());
                    std::fs::create_dir_all(&profile).map_err(|error| error.to_string())?;
                    std::fs::write(profile.join("started"), installed)
                        .map_err(|error| error.to_string())
                }
            '''))
            version_provider = root / "fixture-core"
            (version_provider / "src").mkdir(parents=True)
            (version_provider / "Cargo.toml").write_text(
                '[package]\nname="kurisu-core"\nversion="1.0.0"\nedition="2024"\n')
            (version_provider / "src/lib.rs").write_text(
                'pub mod updater { pub fn current_version() -> &\'static str { env!("KURISU_BUILD_VERSION") } }')
            with (root / "Cargo.toml").open("a") as manifest:
                manifest.write('[dependencies]\nlog="0.4"\nenv_logger="0.11"\n'
                               'kurisu-core={path="fixture-core"}\n')
            BUILD.subprocess.run(["cargo", "generate-lockfile", "--offline", "--manifest-path",
                                  str(root / "Cargo.toml")], check=True, env=environment,
                                 capture_output=True, timeout=60)
            def notices(destination, offline, selected=None):
                self.assertEqual(selected, target)
                (destination / "LICENSE").write_text(license_text)
                (destination / "THIRD_PARTY_NOTICES.txt").write_text("current embedded dependency texts")
            environment["CARGO_BUILD_TARGET"] = "unsupported-environment-target"
            running = BUILD.subprocess.Popen([binary], stdout=BUILD.subprocess.PIPE, text=True)
            try:
                self.assertTrue(select.select([running.stdout], [], [], 5)[0], "old binary did not start")
                self.assertEqual(running.stdout.readline(), "old\n")
                with patch.object(BUILD, "ROOT", root), patch.object(BUILD, "write_notices", side_effect=notices), \
                        patch.dict(os.environ, environment, clear=True):
                    self.assertEqual(BUILD.main(["--offline", "--build-version", "1.2.3"]), 0)
                self.assertIsNone(running.poll())
            finally:
                running.terminate()
                running.wait(timeout=5)
                running.stdout.close()
            with tarfile.open(root / "target/release/bundle/Kurisu_1.2.3_linux-x64.tar.gz") as archive:
                self.assertEqual(archive.extractfile("Kurisu/kurisu").read(), binary.read_bytes())
            # Frozen bridge clients install only these executable bytes.
            installed = root / "bare-install"
            installed.mkdir()
            payload = installed / "kurisu"
            shutil.copy2(binary, payload)
            profile = root / "disposable-profile"

            def launch(*arguments, status=0):
                result = BUILD.subprocess.run([payload, *arguments], capture_output=True,
                                              text=True, env=dict(environment, RUST_LOG="warn"), timeout=5)
                self.assertEqual(result.returncode, status, result.stderr)
                return result

            for option, expected in (("--license", license_text),
                                     ("--notices", "current embedded dependency texts"),
                                     ("--version", "kurisu-native 1.2.3\n")):
                with self.subTest(option=option):
                    result = launch("--data-dir", profile, option)
                    self.assertEqual(result.stdout, expected)
                    self.assertEqual(result.stderr, "")
            help_text = launch("--help").stdout
            for option in ("--license", "--notices", "--version", "--data-dir"):
                self.assertIn(option, help_text)
            self.assertIn("--data-dir requires a path", launch("--data-dir", status=2).stderr)
            self.assertIn("Unknown option", launch("--unknown", status=2).stderr)
            self.assertFalse(profile.exists())
            self.assertEqual({path.name for path in installed.iterdir()}, {"kurisu"})
            originals = {}
            for name in ("LICENSE", "THIRD_PARTY_NOTICES.txt", ".kurisu-notices-unrelated"):
                (installed / name).write_text(f"user text for {name}")
                originals[name] = (installed / name).stat().st_ino
            self.assertEqual(launch("--data-dir", profile).stderr, "")
            self.assertEqual((profile / "started").read_text(), "true:true")
            self.assertEqual((installed / "kurisu.LICENSE").read_text(), license_text)
            self.assertEqual((installed / "kurisu.THIRD_PARTY_NOTICES.txt").read_text(), "current embedded dependency texts")
            (installed / "kurisu.THIRD_PARTY_NOTICES.txt").write_text("user edited companion")
            companion_inode = (installed / "kurisu.THIRD_PARTY_NOTICES.txt").stat().st_ino
            (profile / "started").unlink()
            launch("--data-dir", profile)
            self.assertEqual((profile / "started").read_text(), "true:true")
            self.assertEqual((installed / "kurisu.THIRD_PARTY_NOTICES.txt").read_text(), "user edited companion")
            self.assertEqual((installed / "kurisu.THIRD_PARTY_NOTICES.txt").stat().st_ino, companion_inode)
            self.assertEqual(launch("--notices").stdout, "current embedded dependency texts")
            for name in ("LICENSE", "THIRD_PARTY_NOTICES.txt", ".kurisu-notices-unrelated"):
                self.assertEqual((installed / name).read_text(), f"user text for {name}")
                self.assertEqual((installed / name).stat().st_ino, originals[name])
            self.assertEqual({path.name for path in installed.iterdir()}, {
                "kurisu", "LICENSE", "THIRD_PARTY_NOTICES.txt", ".kurisu-notices-unrelated",
                "kurisu.LICENSE", "kurisu.THIRD_PARTY_NOTICES.txt",
            })
            if os.geteuid() != 0:
                (installed / "kurisu.LICENSE").unlink()
                (installed / "kurisu.THIRD_PARTY_NOTICES.txt").unlink()
                (profile / "started").unlink()
                installed.chmod(0o555)
                try:
                    self.assertEqual(launch("--license").stdout, license_text)
                    self.assertEqual(launch("--notices").stdout, "current embedded dependency texts")
                    self.assertFalse((profile / "started").exists())
                    self.assertIn("Could not create companion notices", launch("--data-dir", profile).stderr)
                    self.assertEqual((profile / "started").read_text(), "false:false")
                    self.assertFalse((installed / "kurisu.LICENSE").exists())
                    self.assertFalse((installed / "kurisu.THIRD_PARTY_NOTICES.txt").exists())
                finally:
                    installed.chmod(0o755)

    @unittest.skipUnless(sys.platform == "linux", "Standalone Linux build script")
    def test_companion_notices_preserve_shared_directory_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "THIRD_PARTY_NOTICES.txt").write_text("fixture dependency texts")
            harness = root / "notices-tests"
            BUILD.subprocess.run(["rustc", "--edition=2024", "--test", str(BUILD.ROOT / "src/notices.rs"),
                                  "-o", str(harness)], check=True, env=dict(os.environ, OUT_DIR=str(root)))
            BUILD.subprocess.run([harness], check=True)

    @unittest.skipUnless(sys.platform == "linux", "Standalone Linux build script")
    def test_release_build_script_requires_nonempty_embedded_notices(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / "build-script"
            BUILD.subprocess.run(["rustc", str(BUILD.ROOT / "build.rs"), "-o", str(script)], check=True)
            environment = dict(os.environ, OUT_DIR=str(root), PROFILE="release")
            environment.pop("KURISU_BUNDLED_NOTICES", None)
            result = BUILD.subprocess.run([script], env=environment, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Release builds require dependency notices", result.stderr)
            source = root / "source-notices"
            source.write_text("")
            environment["KURISU_BUNDLED_NOTICES"] = str(source)
            result = BUILD.subprocess.run([script], env=environment, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("dependency notices are empty", result.stderr)
            source.write_text("current dependency notices")
            BUILD.subprocess.run([script], env=environment, check=True, capture_output=True)
            self.assertEqual((root / "THIRD_PARTY_NOTICES.txt").read_text(), "current dependency notices")

    def test_linux_packaging_refuses_an_arm_host_before_building(self):
        response = BUILD.subprocess.CompletedProcess([], 0, stdout="host: aarch64-unknown-linux-gnu\n")
        with patch.object(BUILD.sys, "platform", "linux"), \
                patch.object(BUILD.subprocess, "run", return_value=response) as run:
            with self.assertRaisesRegex(ValueError, "x86_64 Linux host"):
                BUILD.build_target()
            self.assertEqual(run.call_count, 1)

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


class InstallerVersionTests(unittest.TestCase):
    def run_installed_check(self, output, exit_code=0):
        script = (BUILD.ROOT.parent / ".github/test-native-installer.ps1").read_text()
        code = script.split("\n'@ | python - $binary", 1)[0].rsplit("@'\n", 1)[1]
        original_run = BUILD.subprocess.run

        def version_process(command, **options):
            self.assertEqual(command, ["/fixture/kurisu.exe", "--version"])
            return original_run(
                [sys.executable, "-c",
                 f"import sys; sys.stdout.write({output!r}); sys.exit({exit_code})"],
                **options,
            )

        with patch.object(sys, "argv", ["-", "/fixture/kurisu.exe"]), \
                patch.object(BUILD.subprocess, "run", side_effect=version_process):
            exec(compile(code, "installer-version-check", "exec"), {"__name__": "__main__"})

    def test_installed_version_accepts_completed_native_output(self):
        self.run_installed_check("kurisu-native 7.8.9\n")

    def test_installed_version_rejects_failed_process_even_with_valid_output(self):
        with self.assertRaises(BUILD.subprocess.CalledProcessError) as error:
            self.run_installed_check("kurisu-native 7.8.9\n", 7)
        self.assertEqual(error.exception.returncode, 7)
        self.assertEqual(error.exception.stdout, "kurisu-native 7.8.9\n")

    def test_installed_version_rejects_empty_or_wrong_output(self):
        for output in ("", "kurisu 7.8.9\n", "kurisu-native 7.8.9\nunexpected\n"):
            with self.subTest(output=output), self.assertRaisesRegex(
                    SystemExit, "Installed executable did not report a native version"):
                self.run_installed_check(output)


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

    def test_windows_notices_use_binary_target_on_arm_host(self):
        metadata = self.metadata([self.package("app")], {})

        def tool_output(command, **options):
            if command[0] == "rustc":
                output = "host: aarch64-pc-windows-msvc\n"
            else:
                target = command[command.index("--filter-platform") + 1]
                self.assertEqual(target, "x86_64-pc-windows-msvc")
                output = json.dumps(metadata)
            return BUILD.subprocess.CompletedProcess(command, 0, stdout=output)

        with patch.object(BUILD.sys, "platform", "win32"), \
                patch.object(BUILD.subprocess, "run", side_effect=tool_output), \
                patch.object(BUILD, "collect_notices", return_value="notices"):
            BUILD.write_notices(self.root, offline=True)

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

        with patch.object(BUILD.sys, "platform", "win32"), \
                patch.object(BUILD.subprocess, "run", side_effect=tool_output), \
                patch.object(BUILD.subprocess, "_text_encoding", return_value="cp1252"), \
                patch.object(BUILD, "collect_notices", side_effect=lambda value: original_notices(value, self.supplements)):
            BUILD.write_notices(self.root, offline=True)
        notices = (self.root / "THIRD_PARTY_NOTICES.txt").read_text(encoding="utf-8")
        self.assertIn("Authors: Māori author", notices)
        self.assertIn("Permission for Māori authors", notices)


if __name__ == "__main__":
    unittest.main()
