import { readFileSync, appendFileSync, readdirSync, statSync } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { pathToFileURL } from "node:url";

const MAX_BUFFER = 64 * 1024 * 1024;

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

export function plan(tag) {
  version(tag);
  return { tag, ver: tag.slice(1) };
}

export function requiredNativeAssets(tag) {
  const ver = plan(tag).ver;
  return ["kurisu-native", "kurisu-native.exe", `Kurisu-Native_${ver}_x64-install.exe`,
    `Kurisu_${ver}_linux-x64.tar.gz`].flatMap(name => [name, `${name}.sha256`]);
}

export function nativeAssets(names, tag) {
  const required = requiredNativeAssets(tag);
  if (required.some(name => !names.includes(name))) throw new Error("Both native platforms and Linux bundle are required");
  if (names.some(name => !required.includes(name))) throw new Error("Unexpected release assets");
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

async function waitForReleaseList(tag, id, draft) {
  const state = draft ? "draft" : "published";
  // GitHub's release list can lag behind a successful create or publish.
  for (const delay of [0, 1000, 2000, 4000, 8000, 16000, 32000]) {
    if (delay) await sleep(delay);
    const current = existingRelease(tag);
    if (current?.id === id && current.draft === draft) return;
    const listed = current ? `${current.draft ? "draft" : "published"} release ${current.id}` : "no release";
    if (current && (current.id !== id || draft)) {
      throw new Error(`Release changed for ${tag}. Expected ${state} release ${id}, found ${listed}.`);
    }
    if (!delay) console.log(`GitHub lists ${listed} for ${tag}. Waiting for ${state} release ${id}.`);
  }
  throw new Error(`Timed out waiting for GitHub to list ${tag} as ${state} release ${id}. ${retryArtifacts}`);
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
  return `Kurisu ${tag.slice(1)}. Linux: download Kurisu_${tag.slice(1)}_linux-x64.tar.gz. Windows: download Kurisu-Native_${tag.slice(1)}_x64-install.exe.`;
}

async function main() {
  const [command, tag, directory] = process.argv.slice(2);
  const result = plan(tag);
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
  const names = nativeAssets(paths, tag);
  const expected = verifySidecars(absoluteDirectory, names);
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
    if (!Number.isSafeInteger(id) || id <= 0) throw new Error("Draft creation returned an invalid release ID");
    await waitForReleaseList(tag, id, true);
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
  await waitForReleaseList(tag, id, false);
  verifyPublished();
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
