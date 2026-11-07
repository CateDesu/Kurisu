import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdtempSync, mkdirSync, rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { bridgeAssets, compare, nativeAssets, originalWindowsSelector, plan, transition } from "./release-transition.mjs";

const installer = "Kurisu_1.0.0_x64-setup.exe";
const bridgeNames = ["kurisu", "kurisu.exe", installer].flatMap(name => [name, `${name}.sha256`]);
const bridge = () => ({ tag_name: transition.bridgeTag, draft: false, prerelease: false,
  assets: bridgeNames.map(name => ({ name })) });
const nativeNames = tag => ["kurisu-native", "kurisu-native.exe",
  `Kurisu-Native_${tag.slice(1)}_x64-install.exe`, `Kurisu_${tag.slice(1)}_linux-x64.tar.gz`]
  .flatMap(name => [name, `${name}.sha256`]);

// Exact shipped matching rule, including its original lack of a product prefix.
function publishedWindowsSelector(names, arch = "x86_64") {
  const marker = arch === "x86_64" ? "_x64-" : null;
  if (!marker) return null;
  const hits = names.filter(name => name.endsWith("-setup.exe") && name.includes(marker));
  return hits.length === 1 ? hits[0] : null;
}

test("only the pinned final bridge and higher native versions are allowed", () => {
  assert.equal(plan("bridge", transition.bridgeTag).bridge, "v1.0.0.78");
  assert.equal(plan("native", "v1.1.0").ver, "1.1.0");
  for (const tag of ["v1.0.0.77", "v1.0.0.79"]) assert.throws(() => plan("bridge", tag));
  for (const tag of ["v1.0.0.77", transition.bridgeTag, "v1.1.0-beta", "v1.1.65536", "v1.1.0\n"]) {
    assert.throws(() => plan("native", tag));
  }
  assert.equal(compare("v1.1.0", "v1.1.0.0"), 0);
});

test("the frozen bridge requires both platforms, one installer and existing sidecars", () => {
  assert.deepEqual(bridgeAssets(bridge()), bridgeNames);
  for (const field of ["draft", "prerelease"]) assert.throws(() => bridgeAssets({ ...bridge(), [field]: true }));
  assert.throws(() => bridgeAssets({ ...bridge(), tag_name: "v1.0.0.77" }));
  for (const missing of bridgeNames) {
    assert.throws(() => bridgeAssets({ ...bridge(), assets: bridge().assets.filter(asset => asset.name !== missing) }));
  }
  assert.throws(() => bridgeAssets({ ...bridge(), assets: [...bridge().assets, { name: "kurisu-native" }] }));
  assert.throws(() => bridgeAssets({ ...bridge(), assets: [...bridge().assets, { name: "Other_x64-setup.exe" }] }));
});

test("users skipping the bridge still see exactly its old installer on every native release", () => {
  for (const tag of ["v1.1.0", "v1.2.3", "v2.0.0.5"]) {
    const names = [...bridgeAssets(bridge()), ...nativeAssets(nativeNames(tag), tag)];
    assert.equal(publishedWindowsSelector(names), installer);
    assert.equal(originalWindowsSelector(names), publishedWindowsSelector(names));
    assert.equal(publishedWindowsSelector(names, "aarch64"), null);
    assert.equal(names.filter(name => name === "kurisu").length, 1);
    assert.ok(names.includes("kurisu-native"));
  }
  assert.equal(publishedWindowsSelector([...bridgeNames, "Kurisu-Native_1.1.0_x64-setup.exe"]), null);
  assert.throws(() => nativeAssets([...nativeNames("v1.1.0"), "Kurisu-Native_1.1.0_x64-setup.exe"], "v1.1.0"));
  assert.throws(() => nativeAssets(nativeNames("v1.1.0").slice(1), "v1.1.0"));
});

test("publishing is manual, default-off and never prunes the pinned bridge", () => {
  for (const name of ["windows-build.yml", "native-release.yml"]) {
    const workflow = readFileSync(new URL(`workflows/${name}`, import.meta.url), "utf8");
    assert.match(workflow, /workflow_dispatch:/);
    assert.doesNotMatch(workflow, /^  (push|release):/m);
    assert.match(workflow, /publish:[\s\S]*?type: boolean\r?\n\s+default: false/);
    assert.match(workflow, /if: inputs.publish/);
    assert.doesNotMatch(workflow, /release delete|cleanup-tag|prune-rolling|--clobber/);
  }
  const workflow = readFileSync(new URL("workflows/native-release.yml", import.meta.url), "utf8");
  assert.match(workflow, /source-ref: \$\{\{ inputs.tag \}\}/);
});

function publicationFixture(context) {
  const directory = mkdtempSync(join(tmpdir(), "kurisu-release-proof-"));
  context.after(() => rmSync(directory, { recursive: true, force: true }));
  const bin = join(directory, "bin");
  const source = join(directory, "frozen-bridge");
  const stage = join(directory, "stage");
  for (const path of [bin, source, stage]) mkdirSync(path);
  for (const name of [...bridgeNames.filter(name => !name.endsWith(".sha256")),
    ...nativeNames("v1.1.0").filter(name => !name.endsWith(".sha256"))]) {
    const target = name.startsWith("kurisu-native") || name.endsWith("-install.exe") || name.endsWith(".tar.gz") ? stage : source;
    const payload = Buffer.from(`immutable payload for ${name}`);
    writeFileSync(join(target, name), payload);
    writeFileSync(join(target, `${name}.sha256`), `${createHash("sha256").update(payload).digest("hex")}  ${name}\n`);
  }
  writeFileSync(join(directory, "bridge.json"), JSON.stringify(bridge()));
  writeFileSync(join(bin, "git"), "#!/bin/sh\nprintf '0123456789abcdef\\n'\n", { mode: 0o755 });
  writeFileSync(join(bin, "gh"), `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const args = process.argv.slice(2);
fs.appendFileSync(process.env.KURISU_TEST_LOG, JSON.stringify(args) + "\\n");
if (args[0] === "api") {
  if (args.includes("--repo")) throw new Error("gh api does not accept --repo");
  console.log(fs.readFileSync(process.env.KURISU_TEST_BRIDGE, "utf8"));
} else if (args[1] === "list") {
  console.log(JSON.stringify([{tagName:"v1.0.0.78",isDraft:false,isPrerelease:false}]));
} else if (args[1] === "download") {
  const destination = args[args.indexOf("--dir") + 1];
  for (let index = 0; index < args.length; index++) if (args[index] === "--pattern") {
    const name = args[index + 1];
    fs.copyFileSync(path.join(process.env.KURISU_TEST_SOURCE, name), path.join(destination, name));
  }
}
`, { mode: 0o755 });
  const log = join(directory, "commands.jsonl");
  const execute = () => spawnSync(process.execPath, [fileURLToPath(new URL("release-transition.mjs", import.meta.url)),
    "publish", "native", "v1.1.0", stage], { cwd: directory, encoding: "utf8", env: {
      ...process.env, PATH: `${bin}:${process.env.PATH}`, KURISU_TEST_LOG: log,
      KURISU_TEST_SOURCE: source, KURISU_TEST_BRIDGE: join(directory, "bridge.json"),
    } });
  const commands = () => readFileSync(log, "utf8").trim().split("\n").map(line => JSON.parse(line));
  return { directory, source, stage, log, execute, commands };
}

test("publication copies frozen bridge bytes and verifies existing sidecars before creating a draft", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute();
  assert.equal(result.status, 0, result.stderr);
  for (const name of bridgeNames) {
    assert.deepEqual(readFileSync(join(fixture.stage, name)), readFileSync(join(fixture.source, name)));
  }
  const commands = fixture.commands();
  assert.deepEqual(commands.filter(args => args[1] === "create").length, 1);
  assert.ok(commands.find(args => args[1] === "create").includes("--verify-tag"));
  const upload = commands.find(args => args[1] === "upload");
  assert.ok(upload.includes(join(fixture.stage, installer)));
  assert.ok(upload.includes(join(fixture.stage, "Kurisu-Native_1.1.0_x64-install.exe")));
  assert.ok(!upload.includes("--clobber"));
  assert.equal(commands.at(-1)[1], "edit");
  assert.ok(commands.at(-1).includes("--draft=false"));
});

test("damaged frozen bridge refuses publication without creating or editing a release", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  writeFileSync(join(fixture.source, "kurisu"), "damaged after original checksum was written");
  const result = fixture.execute();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /checksum did NOT match|FAILED/);
  assert.ok(!fixture.commands().some(args => ["create", "upload", "edit"].includes(args[1])));
});

test("a draft or missing bridge asset refuses native publication before any write", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  writeFileSync(join(fixture.directory, "bridge.json"), JSON.stringify({ ...bridge(), draft: true }));
  const result = fixture.execute();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /published stable release/);
  assert.ok(!fixture.commands().some(args => ["create", "download", "upload", "edit"].includes(args[1])));
});
