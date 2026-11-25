import { readFileSync, appendFileSync, readdirSync, statSync, copyFileSync, constants, existsSync, mkdtempSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const MAX_BUFFER = 64 * 1024 * 1024;
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
  if (kind !== "native") throw new Error("Only native releases can be built or published");
  if (compare(tag, transition.bridgeTag) <= 0) throw new Error("Native release must be newer than the pinned bridge");
  return { tag, ver: tag.slice(1) };
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

function run(program, args, cwd) {
  const result = spawnSync(program, args, { cwd, encoding: "utf8", maxBuffer: MAX_BUFFER, stdio: ["ignore", "pipe", "pipe"] });
  if (result.error || result.status !== 0) throw new Error(result.error?.message || result.stderr || `${program} failed`);
  return result.stdout.trim();
}

function repository() {
  return process.env.GITHUB_REPOSITORY || "CateDesu/Kurisu";
}

function gh(args) {
  return run("gh", [...args, "--repo", repository()]);
}

function parseJsonLines(text) {
  return text ? text.split("\n").filter(Boolean).map(line => JSON.parse(line)) : [];
}

function existingRelease(tag) {
  const releases = parseJsonLines(run("gh", [
    "api", "--paginate", "--jq", ".[] | {id, tag_name, draft, prerelease}",
    `repos/${repository()}/releases?per_page=100`,
  ]));
  const matches = releases.filter(release => release.tag_name === tag);
  if (matches.length > 1 || matches.some(release => typeof release.draft !== "boolean" || release.prerelease !== false
      || !Number.isSafeInteger(release.id) || release.id <= 0)) {
    throw new Error("Release already exists without a unique stable release or draft");
  }
  for (const release of releases.filter(release => !release.draft && !release.prerelease)) {
    if (release.tag_name !== tag && /^v\d+\.\d+\.\d+(?:\.\d+)?$/.test(release.tag_name) && compare(release.tag_name, tag) >= 0) {
      throw new Error("A stable release at this or a higher version is already published");
    }
  }
  return matches[0];
}

function verifiedAssets(release, expected) {
  const label = release.draft ? "Draft" : "Published release";
  const uploaded = new Set();
  const incomplete = new Map();
  for (const asset of release.assets) {
    const local = expected.get(asset.name);
    if (!local || uploaded.has(asset.name) || incomplete.has(asset.name)) {
      throw new Error(`Unexpected ${release.draft ? "draft" : "published"} asset: ${asset.name}`);
    }
    if (release.draft && asset.state === "starter" && asset.size === 0 && !asset.digest
        && Number.isSafeInteger(asset.id) && asset.id > 0) {
      incomplete.set(asset.name, asset.id);
    } else if (asset.state === "uploaded" && asset.size === local.size && asset.digest === local.digest) {
      uploaded.add(asset.name);
    } else {
      throw new Error(`${label} asset does not match the verified build: ${asset.name}. ${retryArtifacts}`);
    }
  }
  return { uploaded, incomplete };
}

const retryArtifacts = "Use the original verified build artifacts. In GitHub Actions, re-run failed jobs from the original Native release run.";

function verifyPinnedBridgeInstaller(directory, installerName) {
  if (!/^[0-9a-f]{64}$/.test(transition.bridgeInstallerSha256)) {
    throw new Error("Pinned bridge installer SHA-256 is missing or invalid");
  }
  const actual = createHash("sha256").update(readFileSync(join(directory, installerName))).digest("hex");
  if (actual !== transition.bridgeInstallerSha256) {
    throw new Error("Bridge installer SHA-256 does not match the pinned transition digest");
  }
}

function verifySidecars(directory, names) {
  const assets = new Map();
  for (const name of names.filter(name => !name.endsWith(".sha256"))) {
    const path = join(directory, name);
    const size = statSync(path).size;
    if (size === 0) throw new Error(`Empty release asset: ${name}`);
    const checksum = `${name}.sha256`;
    const content = readFileSync(join(directory, checksum));
    const sidecar = content.toString("utf8").trim();
    if (!new RegExp(`^[0-9a-f]{64}  ${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`).test(sidecar)) {
      throw new Error(`Invalid existing checksum sidecar: ${name}`);
    }
    run("sha256sum", ["--check", "--strict", checksum], directory);
    assets.set(name, { size, digest: `sha256:${sidecar.slice(0, 64)}` });
    assets.set(checksum, { size: content.length, digest: `sha256:${createHash("sha256").update(content).digest("hex")}` });
  }
  return assets;
}

function stageBridge(directory, names) {
  const temporary = mkdtempSync(join(tmpdir(), "kurisu-release-bridge-"));
  try {
    gh(["release", "download", transition.bridgeTag, "--dir", temporary, ...names.flatMap(name => ["--pattern", name])]);
    const verified = verifySidecars(temporary, names);
    verifyPinnedBridgeInstaller(temporary, originalWindowsSelector(names));
    for (const name of names) {
      const path = join(directory, name);
      if (!existsSync(path)) copyFileSync(join(temporary, name), path, constants.COPYFILE_EXCL);
    }
    const staged = verifySidecars(directory, names);
    for (const [name, { digest }] of verified) {
      if (staged.get(name).digest !== digest) throw new Error(`Staged asset differs from the pinned bridge: ${name}`);
    }
    return staged;
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

function verifyRemoteTagCommit(tag, expectedCommit) {
  // GitHub ignores target_commitish for an existing release tag.
  const git = `repos/${repository()}/git`;
  let { object } = JSON.parse(run("gh", ["api", `${git}/ref/tags/${encodeURIComponent(tag)}`]));
  while (object.type === "tag") {
    ({ object } = JSON.parse(run("gh", ["api", `${git}/tags/${object.sha}`])));
  }
  if (object.type !== "commit" || object.sha.toLowerCase() !== expectedCommit.toLowerCase()) {
    throw new Error("Remote release tag no longer points to the planned commit");
  }
}

function releaseNotes(tag) {
  return `Kurisu ${tag.slice(1)}. Fresh Linux installs: download Kurisu_${tag.slice(1)}_linux-x64.tar.gz. Fresh Windows installs: download Kurisu-Native_${tag.slice(1)}_x64-install.exe. Ordinary upgrades need no Python. Existing native profiles remain separate from legacy profiles.\n\nLegacy assets named kurisu, kurisu.exe and Kurisu_*-setup.exe are the unchanged ${transition.bridgeTag} transition bridge for old updaters. They are kept intentionally; fresh installs should use the native downloads above. Keep the pinned bridge release for rollback. Changes made only in the native profile are not copied back to the legacy profile.`;
}

async function main() {
  const [command, kind, tag, directory] = process.argv.slice(2);
  const result = plan(kind, tag);
  const commit = run("git", ["rev-parse", `refs/tags/${tag}^{commit}`]);
  const plannedCommit = process.env.RELEASE_COMMIT;
  if (command === "publish" && plannedCommit !== undefined) {
    if (!/^[0-9a-f]{40}$/i.test(plannedCommit)) throw new Error("Invalid planned release commit");
    if (commit !== plannedCommit.toLowerCase()) throw new Error("Release tag no longer points to the planned commit");
  }
  if (commit !== run("git", ["rev-parse", "HEAD"])) throw new Error("Build the exact tagged source commit");
  if (command === "plan") {
    const outputs = { ...result, commit };
    if (process.env.GITHUB_OUTPUT) for (const [key, value] of Object.entries(outputs)) {
      appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${value}\n`);
    }
    console.log(JSON.stringify(outputs));
    return;
  }
  if (command !== "publish" || !directory) throw new Error("Use plan or publish with an artifact directory");
  const endpoint = `repos/${repository()}/releases`;
  const existing = existingRelease(tag);
  let id = existing?.id;
  const title = `Kurisu ${result.ver}`;
  const notes = releaseNotes(tag);
  const absoluteDirectory = run("realpath", [directory]);
  const paths = readdirSync(absoluteDirectory).filter(name => statSync(join(absoluteDirectory, name)).isFile());
  const bridge = JSON.parse(run("gh", ["api", `${endpoint}/tags/${transition.bridgeTag}`]));
  const legacy = bridgeAssets(bridge);
  const names = nativeAssets(paths.filter(name => !legacy.includes(name)), tag);
  const expected = new Map([...verifySidecars(absoluteDirectory, names), ...stageBridge(absoluteDirectory, legacy)]);
  const manifest = Array.from(expected, ([name, { size, digest }]) => [name, size, digest]);
  const fingerprint = createHash("sha256").update(JSON.stringify(manifest)).digest("hex");
  const marker = `${notes}\n\n<!-- Kurisu native release for ${commit} with assets `;
  const body = `${marker}${fingerprint} -->`;
  function readOwned(draft) {
    const release = JSON.parse(run("gh", ["api", `${endpoint}/${id}`]));
    const type = draft ? "native draft" : "published native release";
    if (release?.id !== id || release.tag_name !== tag || release.draft !== draft || release.prerelease !== false
        || release.name !== title || !Array.isArray(release.assets)) {
      throw new Error(`Release already exists and is not the expected ${type}`);
    }
    if (release.body !== body) {
      if (typeof release.body === "string" && release.body.startsWith(marker)
          && /^[0-9a-f]{64} -->$/.test(release.body.slice(marker.length))) {
        throw new Error(`Release artifacts differ from the original verified build. ${retryArtifacts}`);
      }
      throw new Error(`Release already exists and is not the expected ${type}`);
    }
    return release;
  }
  function matchesRelease(draft) {
    const current = existingRelease(tag);
    return current?.id === id && current?.draft === draft;
  }
  function verifyPublished() {
    if (!matchesRelease(false)) throw new Error("Published release identity changed");
    if (verifiedAssets(readOwned(false), expected).uploaded.size !== expected.size) {
      throw new Error("Published release is missing verified assets");
    }
    verifyRemoteTagCommit(tag, commit);
  }
  if (!matchesRelease(existing?.draft)) throw new Error("Release changed during staging");
  if (existing?.draft === false) {
    verifyPublished();
    console.log(`Verified previously published release ${tag}`);
    return;
  }
  verifyRemoteTagCommit(tag, commit);
  if (id === undefined) {
    const created = JSON.parse(run("gh", ["api", "--method", "POST", endpoint,
      "-f", `tag_name=${tag}`, "-f", `target_commitish=${commit}`, "-f", `name=${title}`, "-f", `body=${body}`, "-F", "draft=true"]));
    id = created?.id;
    if (!Number.isSafeInteger(id) || id <= 0 || !matchesRelease(true)) {
      throw new Error("Release changed after draft creation");
    }
  }
  const draft = readOwned(true);
  const uploadTemplate = draft.upload_url;
  const uploadPath = `/${endpoint}/${id}/assets`;
  const uploadHost = "https://uploads.github.com";
  if (typeof uploadTemplate !== "string"
      || uploadTemplate.toLowerCase() !== `${uploadHost}${uploadPath.toLowerCase()}{?name,label}`) {
    throw new Error("Unexpected release asset upload URL");
  }
  const uploadUrl = uploadTemplate.slice(0, -"{?name,label}".length);
  let assets = verifiedAssets(draft, expected);
  verifyRemoteTagCommit(tag, commit);
  for (const name of expected.keys()) {
    if (assets.uploaded.has(name)) continue;
    if (assets.incomplete.has(name)) {
      run("gh", ["api", "--method", "DELETE", `${endpoint}/assets/${assets.incomplete.get(name)}`]);
      assets = verifiedAssets(readOwned(true), expected);
      if (assets.uploaded.has(name)) continue;
    }
    run("gh", ["api", "--method", "POST", `${uploadUrl}?name=${encodeURIComponent(name)}`,
      "--header", "Content-Type: application/octet-stream", "--input", join(absoluteDirectory, name)]);
    assets = verifiedAssets(readOwned(true), expected);
  }
  if (!matchesRelease(true)) throw new Error("Release changed during upload");
  if (verifiedAssets(readOwned(true), expected).uploaded.size !== expected.size) throw new Error("Draft is missing verified assets");
  verifyRemoteTagCommit(tag, commit);
  run("gh", ["api", "--method", "PATCH", `${endpoint}/${id}`, "-F", "draft=false",
    "-f", "make_latest=true", "-f", `target_commitish=${commit}`, "-f", `body=${body}`]);
  verifyPublished();
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
