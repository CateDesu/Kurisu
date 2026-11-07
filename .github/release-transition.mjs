import { readFileSync, appendFileSync, readdirSync, statSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { basename, join } from "node:path";
import { pathToFileURL } from "node:url";

export const transition = JSON.parse(readFileSync(new URL("release-transition.json", import.meta.url), "utf8"));

export function version(tag) {
  if (!/^v\d+\.\d+\.\d+(?:\.\d+)?$/.test(tag)) throw new Error("Use a stable numeric v-prefixed tag");
  const parts = tag.slice(1).split(".").map(Number);
  if (parts.some(part => part > 65535)) throw new Error("Version components must fit Windows fields");
  return [...parts, ...Array(4 - parts.length).fill(0)];
}

export function compare(left, right) {
  const a = version(left);
  const b = version(right);
  const index = a.findIndex((part, index) => part !== b[index]);
  return index < 0 ? 0 : a[index] - b[index];
}

export function plan(kind, tag) {
  version(tag);
  if (kind === "bridge") {
    if (tag !== transition.bridgeTag) throw new Error("Only the pinned final bridge tag may build Tauri");
    if (compare(tag, transition.previousStableTag) <= 0) throw new Error("Bridge must advance the previous stable release");
  } else if (kind === "native") {
    if (compare(tag, transition.bridgeTag) <= 0) throw new Error("Native release must be newer than the pinned bridge");
  } else throw new Error("Unknown release kind");
  return { tag, ver: tag.slice(1), bridge: transition.bridgeTag };
}

// This predicate is shipped in v1.0.0.77 and cannot be changed remotely.
export function originalWindowsSelector(names) {
  const hits = names.filter(name => name.endsWith("-setup.exe") && name.includes("_x64-"));
  return hits.length === 1 ? hits[0] : null;
}

export function bridgeAssets(release) {
  if (release.tag_name !== transition.bridgeTag || release.draft || release.prerelease) {
    throw new Error("The pinned bridge must already be a published stable release");
  }
  const names = release.assets.map(asset => asset.name);
  const installer = originalWindowsSelector(names);
  if (!installer || !/^Kurisu_\d+\.\d+\.\d+(?:\.\d+)?_x64-setup\.exe$/.test(installer)) {
    throw new Error("The frozen bridge must have exactly one legacy Windows installer");
  }
  const required = ["kurisu", "kurisu.exe", installer].flatMap(name => [name, `${name}.sha256`]);
  if (names.length !== new Set(names).size || required.some(name => !names.includes(name))) {
    throw new Error("The frozen bridge is missing a binary or existing checksum sidecar");
  }
  if (names.some(name => name.startsWith("kurisu-native") || name.startsWith("Kurisu-Native_"))) {
    throw new Error("The final bridge release must contain legacy binaries only");
  }
  return required;
}

export function nativeAssets(names, tag) {
  const ver = plan("native", tag).ver;
  const required = ["kurisu-native", "kurisu-native.exe", `Kurisu-Native_${ver}_x64-install.exe`,
    `Kurisu_${ver}_linux-x64.tar.gz`].flatMap(name => [name, `${name}.sha256`]);
  if (required.some(name => !names.includes(name))) throw new Error("Both native platforms and Linux bundle are required");
  if (names.some(name => name.endsWith("-setup.exe"))) throw new Error("Native installers must not match the old updater");
  return required;
}

function run(program, args) {
  const result = spawnSync(program, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  if (result.error || result.status !== 0) throw new Error(result.error?.message || result.stderr || `${program} failed`);
  return result.stdout.trim();
}

function gh(args) {
  return run("gh", [...args, "--repo", process.env.GITHUB_REPOSITORY || "CateDesu/Kurisu"]);
}

function refusePublished(tag) {
  const releases = JSON.parse(gh(["release", "list", "--limit", "100", "--json", "tagName,isDraft,isPrerelease"]));
  if (releases.some(release => release.tagName === tag)) throw new Error("Release already exists; do not overwrite or reuse its assets");
  for (const release of releases.filter(release => !release.isDraft && !release.isPrerelease)) {
    if (/^v\d+\.\d+\.\d+(?:\.\d+)?$/.test(release.tagName) && compare(release.tagName, tag) >= 0) {
      throw new Error("A stable release at this or a higher version is already published");
    }
  }
}

function verifySidecars(directory, names) {
  for (const name of names.filter(name => !name.endsWith(".sha256"))) {
    const sidecar = readFileSync(join(directory, `${name}.sha256`), "utf8").trim();
    if (!new RegExp(`^[0-9a-fA-F]{64}  ${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`).test(sidecar)) {
      throw new Error(`Invalid existing checksum sidecar: ${name}`);
    }
    run("sha256sum", ["--check", "--strict", join(directory, `${name}.sha256`)]);
  }
}

function releaseNotes(kind, tag) {
  if (kind === "bridge") return `Final Tauri transition release. Keep this release and its tag for rollback and legacy updater compatibility. Open Kurisu once to prepare your preferences, then install the native update from Settings. Your original profile and a standalone backup are preserved. Native release publication follows separately.`;
  return `Kurisu ${tag.slice(1)} uses GPUI. Fresh Linux installs: download Kurisu_${tag.slice(1)}_linux-x64.tar.gz. Fresh Windows installs: download Kurisu-Native_${tag.slice(1)}_x64-install.exe. Ordinary upgrades need no Python. Existing native profiles remain separate from legacy profiles.\n\nLegacy assets named kurisu, kurisu.exe and Kurisu_*-setup.exe are the unchanged ${transition.bridgeTag} transition bridge for old updaters. They are kept intentionally; fresh installs should use the native downloads above. Keep the pinned bridge release for rollback. Changes made only in the native profile are not copied back to the legacy profile.`;
}

async function main() {
  const [command, kind, tag, directory] = process.argv.slice(2);
  const result = plan(kind, tag);
  const commit = run("git", ["rev-parse", `refs/tags/${tag}^{commit}`]);
  if (commit !== run("git", ["rev-parse", "HEAD"])) throw new Error("Build the exact tagged source commit");
  if (command === "plan") {
    if (process.env.GITHUB_OUTPUT) for (const [key, value] of Object.entries(result)) {
      appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${value}\n`);
    }
    console.log(JSON.stringify({ ...result, commit }));
    return;
  }
  if (command !== "publish" || !directory) throw new Error("Use plan or publish with an artifact directory");
  refusePublished(tag);
  const absoluteDirectory = run("realpath", [directory]);
  const paths = readdirSync(absoluteDirectory).filter(name => statSync(join(absoluteDirectory, name)).isFile());
  let names;
  if (kind === "native") {
    names = nativeAssets(paths, tag);
    const bridge = JSON.parse(run("gh", ["api", `repos/${process.env.GITHUB_REPOSITORY || "CateDesu/Kurisu"}/releases/tags/${transition.bridgeTag}`]));
    const legacy = bridgeAssets(bridge);
    gh(["release", "download", transition.bridgeTag, "--dir", absoluteDirectory, ...legacy.flatMap(name => ["--pattern", name])]);
    names.push(...legacy);
    if (originalWindowsSelector(names) !== originalWindowsSelector(legacy)) throw new Error("Native assets collide with the published updater");
  } else {
    names = bridgeAssets({ tag_name: tag, draft: false, prerelease: false, assets: paths.map(name => ({ name })) });
  }
  process.chdir(absoluteDirectory);
  verifySidecars(absoluteDirectory, names);
  gh(["release", "create", tag, "--verify-tag", "--title", kind === "native" ? `Kurisu ${result.ver}` : `Kurisu ${result.ver} transition bridge`, "--notes", releaseNotes(kind, tag), "--draft"]);
  gh(["release", "upload", tag, ...names.map(name => join(absoluteDirectory, basename(name)))]);
  gh(["release", "edit", tag, "--draft=false", "--latest"]);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
