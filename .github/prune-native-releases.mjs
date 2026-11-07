import { readFileSync, mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { bridgeAssets, compare, nativeAssets, plan, transition } from "./release-transition.mjs";

const stable = release => !release.draft && !release.prerelease;
const numeric = tag => /^v\d+\.\d+\.\d+(?:\.\d+)?$/.test(tag);
const nativeNames = release => nativeAssets(release.assets.map(asset => asset.name)
  .filter(name => !name.endsWith("-setup.exe")), release.tag_name);

export function retentionPlan(releases, tag) {
  plan("native", tag);
  const keep = releases.find(release => release.tag_name === tag);
  const bridge = releases.find(release => release.tag_name === transition.bridgeTag);
  if (!keep || !stable(keep) || !bridge) throw new Error("Published native release and frozen bridge are required");
  bridgeAssets(bridge);
  nativeNames(keep);
  if (releases.some(release => stable(release) && numeric(release.tag_name) && compare(release.tag_name, tag) > 0)) {
    throw new Error("Only the newest stable release may prune older native releases");
  }
  const remove = releases.filter(release => {
    if (!stable(release) || !numeric(release.tag_name) || compare(release.tag_name, transition.bridgeTag) <= 0 || compare(release.tag_name, tag) >= 0) return false;
    try { nativeNames(release); return true; } catch { return false; }
  });
  if ([keep, bridge, ...remove].some(release => !Number.isSafeInteger(release.id) || release.id <= 0)) {
    throw new Error("Release identity is missing");
  }
  return { keep, bridge, remove };
}

function gh(args) {
  const result = spawnSync("gh", args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  if (result.error || result.status !== 0) throw new Error(result.error?.message || result.stderr || "GitHub command failed");
  return result.stdout.trim();
}

function identity(release) {
  return JSON.stringify([release.id, release.tag_name, release.draft, release.prerelease,
    release.assets.map(({ id, name, size, state, digest, updated_at }) => [id, name, size, state, digest, updated_at])]);
}

export function pruneNativeReleases(tag, directory, execute = gh) {
  const repo = process.env.GITHUB_REPOSITORY || "CateDesu/Kurisu";
  const list = () => JSON.parse(execute(["api", "--paginate", "--slurp", `repos/${repo}/releases?per_page=100`])).flat();
  const initial = retentionPlan(list(), tag);
  const legacy = bridgeAssets(initial.bridge);
  const names = [...nativeNames(initial.keep), ...legacy];
  const temporary = mkdtempSync(join(tmpdir(), "kurisu-release-retention-"));
  try {
    const published = join(temporary, "published");
    const frozen = join(temporary, "bridge");
    for (const path of [published, frozen]) mkdirSync(path);
    for (const [releaseTag, target, files] of [[tag, published, names], [transition.bridgeTag, frozen, legacy]]) {
      execute(["release", "download", releaseTag, "--repo", repo, "--dir", target, ...files.flatMap(name => ["--pattern", name])]);
    }
    for (const name of names) {
      if (!readFileSync(join(published, name)).equals(readFileSync(join(directory, name)))) {
        throw new Error(`Published asset differs from the verified build: ${name}`);
      }
    }
    for (const name of names.filter(name => !name.endsWith(".sha256"))) {
      const digest = createHash("sha256").update(readFileSync(join(published, name))).digest("hex");
      if (readFileSync(join(published, `${name}.sha256`), "utf8").trim() !== `${digest}  ${name}`) {
        throw new Error(`Published checksum does not match: ${name}`);
      }
    }
    for (const name of legacy) {
      if (!readFileSync(join(published, name)).equals(readFileSync(join(frozen, name)))) {
        throw new Error(`Frozen bridge alias changed: ${name}`);
      }
    }
    const removed = [];
    for (const candidate of initial.remove) {
      const current = retentionPlan(list(), tag);
      const target = current.remove.find(release => release.id === candidate.id);
      if (identity(current.keep) !== identity(initial.keep) || identity(current.bridge) !== identity(initial.bridge) || !target || identity(target) !== identity(candidate)) {
        throw new Error("Release assets changed during cleanup; nothing further was deleted");
      }
      execute(["api", "--method", "DELETE", `repos/${repo}/releases/${target.id}`]);
      if (list().some(release => release.id === target.id)) throw new Error(`Release deletion did not land: ${target.tag_name}`);
      removed.push(target.tag_name);
      console.log(`Removed superseded native release ${target.tag_name}; tag retained`);
    }
    return { kept: tag, bridge: transition.bridgeTag, removed, tagsRetained: true };
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const [tag, directory] = process.argv.slice(2);
    if (!tag || !directory) throw new Error("Pass the published tag and verified asset directory");
    console.log(JSON.stringify(pruneNativeReleases(tag, resolve(directory))));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
