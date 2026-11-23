import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { retentionPlan, pruneNativeReleases } from "./prune-native-releases.mjs";
import { originalWindowsSelector, transition } from "./release-transition.mjs";

const installer = "Kurisu_1.0.0_x64-setup.exe";
const legacy = ["kurisu", "kurisu.exe", installer].flatMap(name => [name, `${name}.sha256`]);
const names = tag => ["kurisu-native", "kurisu-native.exe", `Kurisu-Native_${tag.slice(1)}_x64-install.exe`,
  `Kurisu_${tag.slice(1)}_linux-x64.tar.gz`].flatMap(name => [name, `${name}.sha256`]);
const release = (id, tag, native = true) => ({ id, tag_name: tag, draft: false, prerelease: false,
  assets: [...legacy, ...(native ? names(tag) : [])].map((name, index) => ({ id: id * 100 + index, name, state: "uploaded" })) });
const releases = () => [release(1, transition.previousStableTag, false), release(2, transition.bridgeTag, false),
  release(3, "v1.1.0"), release(4, "v1.1.4"), release(5, "v1.1.5")];

test("keep all legacy releases and newest native while allowing skipped upgrades", () => {
  const rows = releases();
  rows.push({ ...release(6, "v1.1.3"), draft: true }, { ...release(7, "v1.1.6"), prerelease: true });
  const result = retentionPlan(rows, "v1.1.5");
  assert.deepEqual(result.remove.map(row => row.tag_name), ["v1.1.0", "v1.1.4"]);
  assert.equal(originalWindowsSelector(result.keep.assets.map(asset => asset.name)), installer);
  assert.equal(result.bridge.tag_name, transition.bridgeTag);
});

test("refuse stale, missing, draft or incomplete replacement releases", () => {
  assert.throws(() => retentionPlan(releases(), "v1.1.4"), /newest/);
  assert.throws(() => retentionPlan(releases().slice(0, -1), "v1.1.5"), /required/);
  for (const field of ["draft", "prerelease"]) {
    const rows = releases(); rows.at(-1)[field] = true;
    assert.throws(() => retentionPlan(rows, "v1.1.5"), /required/);
  }
  const rows = releases(); rows.at(-1).assets.pop();
  assert.throws(() => retentionPlan(rows, "v1.1.5"), /required/);
  const colliding = releases();
  colliding.at(-1).assets.push({ id: 9999, name: "Extra_x64-setup.exe", state: "uploaded" });
  assert.throws(() => retentionPlan(colliding, "v1.1.5"), /frozen bridge Windows installer/);
});

test("refuse a missing bridge and preserve incomplete or unknown old releases", () => {
  assert.throws(() => retentionPlan(releases().filter(row => row.id !== 2), "v1.1.5"), /required/);
  const rows = releases(); rows.find(row => row.id === 3).assets.pop();
  rows.push(release(8, "historical-build", false));
  assert.deepEqual(retentionPlan(rows, "v1.1.5").remove.map(row => row.id), [4]);
});

function fixture(context, fault = "") {
  const directory = mkdtempSync(join(tmpdir(), "kurisu-retention-test-"));
  context.after(() => rmSync(directory, { recursive: true, force: true }));
  let rows = releases();
  const files = new Map();
  for (const name of [...legacy, ...names("v1.1.5")].filter(name => !name.endsWith(".sha256"))) {
    const bytes = Buffer.from(`payload ${name}`);
    files.set(name, bytes);
    files.set(`${name}.sha256`, Buffer.from(`${createHash("sha256").update(bytes).digest("hex")}  ${name}\n`));
  }
  for (const [name, bytes] of files) writeFileSync(join(directory, name), bytes);
  const commands = [];
  let downloaded = false;
  const execute = args => {
    commands.push(args);
    if (args.includes("--paginate")) {
      assert.ok(args.includes("--jq"), "expected --jq projection on paginated release listing");
      assert.ok(!args.includes("--slurp"), "gh api does not allow --slurp with --jq");
      if (downloaded && fault === "newer") rows = [...rows.filter(row => row.id !== 9), release(9, "v1.1.6")];
      if (downloaded && fault === "changed") rows.at(-1).assets[0].id++;
      return rows.map(row => JSON.stringify({
        id: row.id,
        tag_name: row.tag_name,
        draft: row.draft,
        prerelease: row.prerelease,
        assets: row.assets.map(({ id, name, size, state, digest, updated_at }) => ({
          id, name, size, state, digest, updated_at,
        })),
      })).join("\n");
    }
    if (args[0] === "release" && args[1] === "download") {
      const tag = args[2];
      const destination = args[args.indexOf("--dir") + 1];
      for (let index = 0; index < args.length; index++) if (args[index] === "--pattern") {
        const name = args[index + 1];
        let bytes = files.get(name);
        if (fault === "public" && tag === "v1.1.5" && name === "kurisu-native") bytes = Buffer.from("wrong release binary");
        if (fault === "bridge" && tag === transition.bridgeTag && name === "kurisu") bytes = Buffer.from("different frozen bridge");
        if (fault === "missing" && name === "kurisu-native.exe") throw new Error("Asset missing");
        writeFileSync(join(destination, name), bytes);
      }
      downloaded = true;
      return "";
    }
    assert.deepEqual(args.slice(0, 3), ["api", "--method", "DELETE"]);
    assert.match(args[3], /\/releases\/\d+$/);
    const id = Number(args[3].split("/").at(-1));
    if (fault !== "not-deleted") rows = rows.filter(row => row.id !== id);
    return "";
  };
  return { directory, commands, execute, remaining: () => rows };
}

test("verify published bytes and bridge before deleting only old release records", context => {
  const f = fixture(context);
  const result = pruneNativeReleases("v1.1.5", f.directory, f.execute);
  assert.deepEqual(result.removed, ["v1.1.0", "v1.1.4"]);
  assert.deepEqual(f.remaining().map(row => row.tag_name), [transition.previousStableTag, transition.bridgeTag, "v1.1.5"]);
  assert.ok(result.tagsRetained);
  assert.equal(f.commands.filter(args => args.includes("DELETE")).length, 2);
  assert.ok(f.commands.findIndex(args => args.includes("DELETE")) > f.commands.findLastIndex(args => args[1] === "download"));
  assert.ok(!f.commands.some(args => args.some(arg => arg.includes("git/refs") || arg === "--cleanup-tag")));
});

for (const fault of ["public", "bridge", "missing", "newer", "changed"]) {
  test(`${fault} publication failure prevents all deletion`, context => {
    const f = fixture(context, fault);
    assert.throws(() => pruneNativeReleases("v1.1.5", f.directory, f.execute));
    assert.equal(f.commands.filter(args => args.includes("DELETE")).length, 0);
  });
}

test("a deletion that does not land stops cleanup", context => {
  const f = fixture(context, "not-deleted");
  assert.throws(() => pruneNativeReleases("v1.1.5", f.directory, f.execute), /did not land/);
  assert.equal(f.commands.filter(args => args.includes("DELETE")).length, 1);
});

test("pruneNativeReleases honors GITHUB_REPOSITORY and rejects uppercase sidecars", context => {
  const f = fixture(context);
  const sidecar = join(f.directory, "kurisu-native.sha256");
  const original = createHash("sha256").update(Buffer.from("payload kurisu-native")).digest("hex");
  writeFileSync(sidecar, `${original.toUpperCase()}  kurisu-native\n`);
  const prev = process.env.GITHUB_REPOSITORY;
  process.env.GITHUB_REPOSITORY = "ForkOwner/Kurisu";
  try {
    assert.throws(() => pruneNativeReleases("v1.1.5", f.directory, args => {
      const out = f.execute(args);
      if (args[0] === "release" && args[1] === "download" && args[2] === "v1.1.5") {
        const destination = args[args.indexOf("--dir") + 1];
        writeFileSync(join(destination, "kurisu-native.sha256"), `${original.toUpperCase()}  kurisu-native\n`);
      }
      return out;
    }), /Published checksum does not match: kurisu-native/);
    assert.ok(f.commands[0].includes("repos/ForkOwner/Kurisu/releases?per_page=100"));
  } finally {
    if (prev === undefined) delete process.env.GITHUB_REPOSITORY;
    else process.env.GITHUB_REPOSITORY = prev;
  }
});

test("cli execution handles paginated release listings larger than default spawnSync buffer", {
  skip: process.platform === "win32",
}, context => {
  const f = fixture(context);
  const bin = join(f.directory, "bin");
  mkdirSync(bin);
  const releasesPath = join(f.directory, "huge-releases.json");
  const padding = "y".repeat(3_500);
  const manyDrafts = Array.from({ length: 450 }, (_, index) => ({
    id: 1000 + index,
    tag_name: `v1.0.0.${index + 100}-${padding}`,
    draft: true,
    prerelease: false,
    assets: [],
  }));
  writeFileSync(releasesPath, JSON.stringify([...f.remaining(), ...manyDrafts]));
  writeFileSync(join(bin, "gh"), `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const args = process.argv.slice(2);
if (args[0] === "api" && args.includes("--paginate")) {
  if (!args.includes("--jq") || args.includes("--slurp")) process.exit(2);
  const rows = JSON.parse(fs.readFileSync(process.env.KURISU_HUGE_RELEASES, "utf8"));
  for (const row of rows) {
    console.log(JSON.stringify({
      id: row.id,
      tag_name: row.tag_name,
      draft: row.draft,
      prerelease: row.prerelease,
      assets: row.assets,
    }));
  }
} else if (args[0] === "release" && args[1] === "download") {
  const destination = args[args.indexOf("--dir") + 1];
  for (let index = 0; index < args.length; index++) if (args[index] === "--pattern") {
    const name = args[index + 1];
    fs.copyFileSync(path.join(process.env.KURISU_ASSET_DIR, name), path.join(destination, name));
  }
} else if (args[0] === "api" && args.includes("DELETE")) {
  const id = Number(args.at(-1).split("/").at(-1));
  const rows = JSON.parse(fs.readFileSync(process.env.KURISU_HUGE_RELEASES, "utf8")).filter(row => row.id !== id);
  fs.writeFileSync(process.env.KURISU_HUGE_RELEASES, JSON.stringify(rows));
}
`, { mode: 0o755 });
  const cli = spawnSync(process.execPath, [
    fileURLToPath(new URL("prune-native-releases.mjs", import.meta.url)),
    "v1.1.5",
    f.directory,
  ], {
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${bin}:${process.env.PATH}`,
      KURISU_HUGE_RELEASES: releasesPath,
      KURISU_ASSET_DIR: f.directory,
    },
  });
  assert.equal(cli.status, 0, cli.stderr);
  const parsed = JSON.parse(cli.stdout.trim().split("\n").at(-1));
  assert.deepEqual(parsed.removed, ["v1.1.0", "v1.1.4"]);
});
