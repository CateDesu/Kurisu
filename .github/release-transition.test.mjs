import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdtempSync, mkdirSync, rmSync, cpSync, copyFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { bridgeAssets, compare, nativeAssets, originalWindowsSelector, plan, transition } from "./release-transition.mjs";
import { pruneNativeReleases } from "./prune-native-releases.mjs";

const installer = "Kurisu_1.0.0_x64-setup.exe";
const sourceCommit = "0123456789abcdef0123456789abcdef01234567";
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

test("only native releases newer than the published bridge are allowed", () => {
  assert.throws(() => plan("bridge", transition.bridgeTag), /Only native releases/);
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
  const workflow = readFileSync(new URL("workflows/native-release.yml", import.meta.url), "utf8");
  assert.match(workflow, /workflow_dispatch:/);
  assert.doesNotMatch(workflow, /^  (push|release):/m);
  assert.match(workflow, /publish:[\s\S]*?type: boolean\r?\n\s+default: false/);
  assert.match(workflow, /if: inputs.publish/);
  assert.doesNotMatch(workflow, /release delete|cleanup-tag|prune-rolling|--clobber/);
});

test("the Windows installer proof checks the pinned bridge before launch", () => {
  const script = readFileSync(new URL("test-native-installer.ps1", import.meta.url), "utf8");
  assert.match(script, /\$expectedBridgeHash = \[string\]\$transition\.bridgeInstallerSha256\b/);
  assert.match(script, /\$actualBridgeHash -cne \$expectedBridgeHash -or \$actualBridgeHash -cne \$Matches\[1\]/);
  const verifyIndex = script.indexOf("Bridge installer SHA-256 verification failed");
  const launchIndex = script.indexOf("Start-Process -FilePath $BridgeInstaller");
  assert.ok(verifyIndex >= 0, "expected SHA-256 verification error check in installer proof");
  assert.ok(launchIndex > verifyIndex, "installer verification must precede launching the bridge installer");
  assert.equal(transition.bridgeInstallerSha256, "9c16797d7b6dd24bf39465e04f2a266c97cb1f8b401d86897a7cd1d0dfd6112e");
});

function workflowJob(workflow, name) {
  const body = workflow.replaceAll("\r\n", "\n").split(`\n  ${name}:\n`)[1];
  assert.ok(body, `Missing workflow job ${name}`);
  return body.split(/\n  \S/)[0];
}

test("release workflow builds and publishes the planned commit", () => {
  const workflow = readFileSync(new URL("workflows/native-release.yml", import.meta.url), "utf8");
  const [planJob, buildJob, publishJob] = ["plan", "build", "publish"].map(name => workflowJob(workflow, name));
  assert.match(planJob, /commit: \$\{\{ steps\.plan\.outputs\.commit \}\}/);
  assert.match(planJob, /ref: refs\/tags\/\$\{\{ inputs\.tag \}\}\s+fetch-depth: 0/);
  assert.match(planJob, /RELEASE_TAG: \$\{\{ inputs\.tag \}\}\s+run:\s*node \.github\/release-transition\.mjs plan native "\$RELEASE_TAG"/);
  assert.doesNotMatch(planJob, /commit=.*GITHUB_OUTPUT/);
  assert.match(buildJob, /source-ref: \$\{\{ needs\.plan\.outputs\.commit \}\}/);
  assert.match(publishJob, /ref: \$\{\{ needs\.plan\.outputs\.commit \}\}\s+fetch-depth: 0/);
  assert.match(publishJob, /RELEASE_COMMIT: \$\{\{ needs\.plan\.outputs\.commit \}\}\s+run: .* publish native /);
  assert.match(publishJob, /artifact-ids: \$\{\{ needs\.build\.outputs\.linux-artifact-id \}\},\$\{\{ needs\.build\.outputs\.windows-artifact-id \}\}/);
});

test("candidate build and installer proof use the same resolved commit", () => {
  const workflow = readFileSync(new URL("workflows/native-candidate.yml", import.meta.url), "utf8");
  const resolve = workflowJob(workflow, "resolve-source");
  assert.match(workflow, /KURISU_RELEASE_REPOSITORY: \$\{\{ vars\.KURISU_RELEASE_REPOSITORY \|\| 'CateDesu\/Kurisu' \}\}/);
  assert.match(resolve, /ref: \$\{\{ inputs\.source-ref \|\| github\.sha \}\}/);
  assert.match(resolve, /commit: \$\{\{ steps\.source\.outputs\.commit \}\}/);
  assert.match(resolve, /git rev-parse HEAD/);
  for (const name of ["build", "verify-windows-installer"]) {
    assert.match(workflowJob(workflow, name), /ref: \$\{\{ needs\.resolve-source\.outputs\.commit \}\}/);
  }
  assert.match(workflowJob(workflow, "build"), /needs:\s*resolve-source/);
  const verify = workflowJob(workflow, "verify-windows-installer");
  const dependencies = verify.match(/needs:\s*\[([^\]]+)\]/)[1].split(",").map(name => name.trim());
  assert.ok(dependencies.includes("build") && dependencies.includes("resolve-source"));
  assert.match(verify, /artifact-ids: \$\{\{ needs\.build\.outputs\.windows-artifact-id \}\}/);
  assert.match(verify, /--repo \$env:KURISU_RELEASE_REPOSITORY.*--pattern '\*_x64-setup\.exe\.sha256'/);
  assert.match(verify, /test-native-installer\.ps1 -BridgeInstaller/);
  assert.match(workflow, /name: kurisu-native-\$\{\{ runner\.os \}\}-attempt-\$\{\{ github\.run_attempt \}\}/);
  assert.doesNotMatch(workflow, /overwrite: true/);
});

test("candidate concurrency includes the caller workflow and trigger ref", () => {
  const candidate = readFileSync(new URL("workflows/native-candidate.yml", import.meta.url), "utf8");
  const release = readFileSync(new URL("workflows/native-release.yml", import.meta.url), "utf8");
  assert.match(candidate, /^concurrency:\n  group: native-candidate-\$\{\{ github\.workflow \}\}-\$\{\{ github\.ref \}\}\n  cancel-in-progress: true$/m);
  assert.match(release, /^concurrency:\n  group: kurisu-release\n  cancel-in-progress: false$/m);
});

function publicationFixture(context, checkedOutCommit = sourceCommit) {
  const directory = mkdtempSync(join(tmpdir(), "kurisu-release-proof-"));
  context.after(() => rmSync(directory, { recursive: true, force: true }));
  const bin = join(directory, "bin");
  const source = join(directory, "frozen-bridge");
  const artifact = join(directory, "build-artifact");
  const published = join(directory, "published-assets");
  const stage = join(directory, "stage");
  for (const path of [bin, source, artifact, published]) mkdirSync(path);
  for (const name of [...bridgeNames.filter(name => !name.endsWith(".sha256")),
    ...nativeNames("v1.1.0").filter(name => !name.endsWith(".sha256"))]) {
    const target = name.startsWith("kurisu-native") || name.endsWith("-install.exe") || name.endsWith(".tar.gz") ? artifact : source;
    const payload = Buffer.from(`immutable payload for ${name}`);
    writeFileSync(join(target, name), payload);
    writeFileSync(join(target, `${name}.sha256`), `${createHash("sha256").update(payload).digest("hex")}  ${name}\n`);
  }
  const restage = () => {
    rmSync(stage, { recursive: true, force: true });
    cpSync(artifact, stage, { recursive: true });
  };
  restage();
  const pinnedDigest = createHash("sha256").update(readFileSync(join(source, installer))).digest("hex");
  const scriptPath = join(directory, "release-transition.mjs");
  writeFileSync(scriptPath, readFileSync(new URL("release-transition.mjs", import.meta.url)));
  writeFileSync(join(directory, "release-transition.json"), JSON.stringify({
    ...transition,
    bridgeInstallerSha256: pinnedDigest,
  }));
  writeFileSync(join(directory, "bridge.json"), JSON.stringify(bridge()));
  writeFileSync(join(directory, "releases.json"), JSON.stringify([[{ tag_name: "v1.0.0.78", draft: false, prerelease: false }], []]));
  writeFileSync(join(bin, "git"), `#!/bin/sh
if [ -n "$KURISU_TEST_GIT_LOG" ]; then
  printf '%s\\n' "$*" >> "$KURISU_TEST_GIT_LOG"
fi
if [ "$2" = "HEAD" ] && [ -n "$KURISU_TEST_HEAD_COMMIT" ]; then
  printf '%s\\n' "$KURISU_TEST_HEAD_COMMIT"
else
  printf '%s\\n' '${checkedOutCommit}'
fi
`, { mode: 0o755 });
  writeFileSync(join(bin, "gh"), `#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");
const { createHash } = require("node:crypto");
const args = process.argv.slice(2);
const statePath = process.env.KURISU_TEST_DRAFT;
const state = fs.existsSync(statePath) ? JSON.parse(fs.readFileSync(statePath, "utf8")) : null;
const save = () => fs.writeFileSync(statePath, JSON.stringify(state));
fs.appendFileSync(process.env.KURISU_TEST_LOG, JSON.stringify(args) + "\\n");
if (args[0] === "api") {
  if (args.includes("--repo")) throw new Error("gh api does not accept --repo");
  if (args.includes("--slurp") && args.includes("--jq")) {
    throw new Error("the \`--slurp\` option is not supported with \`--jq\` or \`--template\`");
  }
  if (args[2] === "POST" && args[3].endsWith("/releases")) {
    if (state || !args.includes("draft=true")) throw new Error("Invalid draft creation");
    const field = name => args.find(arg => arg.startsWith(name + "=")).slice(name.length + 1);
    const draft = { id: 123, tag_name: field("tag_name"), draft: true, prerelease: false,
      target_commitish: "main", name: field("name"), body: field("body"), assets: [],
      upload_url: "https://uploads.github.com/" + args[3] + "/123/assets{?name,label}" };
    fs.writeFileSync(statePath, JSON.stringify(draft));
    console.log(JSON.stringify(draft));
  } else if (args[2] === "POST" && args[3].startsWith("https://uploads.github.com/")) {
    const endpoint = new URL(args[3]);
    const name = endpoint.searchParams.get("name");
    if (!state?.draft || !endpoint.pathname.endsWith("/releases/" + state.id + "/assets")) throw new Error("Invalid upload target");
    if (!args.includes("Content-Type: application/octet-stream")) throw new Error("Upload requires binary content");
    if (state.assets.some(asset => asset.name === name)) throw new Error("Asset already exists");
    const bytes = fs.readFileSync(args[args.indexOf("--input") + 1]);
    const id = Math.max(0, ...state.assets.map(asset => asset.id)) + 1;
    if (process.env.KURISU_TEST_STARTER && state.assets.length === 1) {
      state.assets.push({ id, name, state: "starter", size: 0, digest: null });
      save();
      throw new Error("synthetic upload failure");
    }
    const asset = { id, name, state: "uploaded", size: bytes.length,
      digest: process.env.KURISU_TEST_NO_UPLOAD_DIGEST ? null : "sha256:" + createHash("sha256").update(bytes).digest("hex") };
    fs.writeFileSync(path.join(process.env.KURISU_TEST_PUBLISHED, name), bytes, { flag: "wx" });
    state.assets.push(asset);
    if (process.env.KURISU_TEST_EDIT_DRAFT_ON_UPLOAD) state.name = "Maintainer draft";
    save();
    if (process.env.KURISU_TEST_FAIL_UPLOAD) throw new Error("synthetic upload failure");
    console.log(JSON.stringify(asset));
  } else if (args[2] === "DELETE") {
    const id = Number(args[3].split("/").at(-1));
    if (!state?.draft || !state.assets.some(asset => asset.state === "starter" && asset.id === id)) {
      throw new Error("Unexpected deletion");
    }
    state.assets = state.assets.filter(asset => asset.id !== id);
    save();
  } else if (args[2] === "PATCH") {
    if (!state?.draft || !args[3].endsWith("/releases/" + state.id)) throw new Error("Invalid publication target");
    state.draft = false;
    state.body = args.find(arg => arg.startsWith("body=")).slice(5);
    save();
    if (process.env.KURISU_TEST_LOSE_PUBLISH_RESPONSE) throw new Error("synthetic lost publish response");
    console.log(JSON.stringify(state));
  } else if (args.includes("--paginate")) {
    const pages = JSON.parse(fs.readFileSync(process.env.KURISU_TEST_RELEASES, "utf8"));
    if (state?.id === 123 && process.env.KURISU_TEST_REPLACE_DRAFT) {
      state.id = 124;
      save();
    }
    if (state) pages.push([state]);
    if (args.includes("--jq")) {
      for (const page of pages) {
        for (const release of page) {
          console.log(JSON.stringify({
            id: release.id,
            tag_name: release.tag_name,
            draft: release.draft,
            prerelease: release.prerelease,
          }));
        }
      }
    } else {
      console.log(fs.readFileSync(process.env.KURISU_TEST_RELEASES, "utf8"));
    }
  } else if (args[1]?.endsWith("/git/ref/tags/v1.1.0")) {
    const uploaded = fs.readFileSync(process.env.KURISU_TEST_LOG, "utf8").trim().split("\\n")
      .some(line => JSON.parse(line)[3]?.startsWith("https://uploads.github.com/"));
    const commit = (uploaded && process.env.KURISU_TEST_COMMIT_AFTER_UPLOAD)
      || process.env.KURISU_TEST_REMOTE_COMMIT || '${checkedOutCommit}';
    const object = process.env.KURISU_TEST_ANNOTATED_TAG
      ? { type: "tag", sha: "c".repeat(40) }
      : { type: "commit", sha: commit };
    console.log(JSON.stringify({ object }));
  } else if (args[1]?.endsWith("/git/tags/" + "c".repeat(40))) {
    console.log(JSON.stringify({ object: { type: "commit", sha: '${checkedOutCommit}' } }));
  } else if (args[1]?.endsWith("/releases/" + state?.id)) {
    console.log(JSON.stringify(state));
  } else {
    console.log(fs.readFileSync(process.env.KURISU_TEST_BRIDGE, "utf8"));
  }
} else if (args[1] === "download") {
  const destination = args[args.indexOf("--dir") + 1];
  for (let index = 0; index < args.length; index++) if (args[index] === "--pattern") {
    const name = args[index + 1];
    fs.copyFileSync(path.join(process.env.KURISU_TEST_SOURCE, name), path.join(destination, name), fs.constants.COPYFILE_EXCL);
  }
} else {
  throw new Error("Unexpected gh invocation: " + args.join(" "));
}
`, { mode: 0o755 });
  const log = join(directory, "commands.jsonl");
  const gitLog = join(directory, "git.log");
  const execute = (extraEnv = {}, command = "publish", tag = "v1.1.0") => spawnSync(process.execPath, [scriptPath,
    command, "native", tag, stage], { cwd: directory, encoding: "utf8", env: {
      ...process.env, PATH: `${bin}:${process.env.PATH}`, KURISU_TEST_LOG: log,
      GITHUB_REPOSITORY: "CateDesu/Kurisu", RELEASE_COMMIT: checkedOutCommit,
      KURISU_TEST_GIT_LOG: gitLog,
      KURISU_TEST_DRAFT: join(directory, "draft.json"),
      KURISU_TEST_PUBLISHED: published,
      KURISU_TEST_SOURCE: source, KURISU_TEST_BRIDGE: join(directory, "bridge.json"),
      KURISU_TEST_RELEASES: join(directory, "releases.json"), ...extraEnv,
    } });
  const commands = () => readFileSync(log, "utf8").trim().split("\n").map(line => JSON.parse(line));
  return { directory, source, stage, published, log, gitLog, execute, commands, restage };
}

const releaseWrite = args => ["POST", "PATCH", "DELETE"].includes(args[2]);
const createDraft = args => args[2] === "POST" && args[3].endsWith("/releases");
const uploadAsset = args => args[2] === "POST" && args[3].startsWith("https://uploads.github.com/");

test("publication refuses a replacement draft with identical metadata after creation", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute({ KURISU_TEST_REPLACE_DRAFT: "1" });
  const draft = JSON.parse(readFileSync(join(fixture.directory, "draft.json")));
  assert.equal(draft.id, 124);
  assert.equal(result.status, 1, "publication accepted a replacement draft");
  assert.match(result.stderr, /Release changed/);
  assert.equal(draft.draft, true);
  assert.deepEqual(draft.assets, []);
  assert.equal(fixture.commands().filter(releaseWrite).length, 1);
});

test("publication stops uploading a draft when its metadata changes", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute({ KURISU_TEST_EDIT_DRAFT_ON_UPLOAD: "1" });
  const draft = JSON.parse(readFileSync(join(fixture.directory, "draft.json")));
  assert.equal(result.status, 1);
  assert.match(result.stderr, /expected native draft/);
  assert.equal(draft.draft, true);
  assert.equal(draft.name, "Maintainer draft");
  assert.equal(draft.assets.length, 1);
  assert.ok(!fixture.commands().some(args => args[2] === "PATCH"));
});

test("a failed upload can resume on the same tag without replacing its uploaded assets", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const first = fixture.execute({ KURISU_TEST_FAIL_UPLOAD: "1" });
  assert.equal(first.status, 1);
  assert.match(first.stderr, /synthetic upload failure/);
  const draftPath = join(fixture.directory, "draft.json");
  const before = JSON.parse(readFileSync(draftPath));
  assert.equal(before.draft, true);
  assert.equal(before.assets.length, 1);

  const retry = fixture.execute();
  assert.equal(retry.status, 0, retry.stderr);
  const after = JSON.parse(readFileSync(draftPath));
  assert.equal(after.id, before.id);
  assert.equal(after.draft, false);
  assert.doesNotMatch(after.body, /Kurisu native release draft for/);
  assert.ok(after.assets.some(asset => JSON.stringify(asset) === JSON.stringify(before.assets[0])));
  assert.deepEqual(after.assets.map(asset => asset.name).sort(), [...bridgeNames, ...nativeNames("v1.1.0")].sort());
  assert.equal(fixture.commands().filter(createDraft).length, 1);
});

test("retry replaces only an empty starter left by a failed upload", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const first = fixture.execute({ KURISU_TEST_STARTER: "1" });
  assert.equal(first.status, 1);
  assert.match(first.stderr, /synthetic upload failure/);
  const draftPath = join(fixture.directory, "draft.json");
  const before = JSON.parse(readFileSync(draftPath));
  const [uploaded, starter] = before.assets;
  assert.equal(starter.state, "starter");

  const retry = fixture.execute();
  assert.equal(retry.status, 0, retry.stderr);
  const after = JSON.parse(readFileSync(draftPath));
  assert.equal(after.draft, false);
  assert.deepEqual(after.assets.find(asset => asset.name === uploaded.name), uploaded);
  assert.equal(after.assets.find(asset => asset.name === starter.name).state, "uploaded");
  assert.deepEqual(fixture.commands().filter(args => args[2] === "DELETE").map(args => args[3]), [
    `repos/CateDesu/Kurisu/releases/assets/${starter.id}`,
  ]);
});

test("publication leaves a draft if GitHub omits an uploaded asset digest", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute({ KURISU_TEST_NO_UPLOAD_DIGEST: "1" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Draft asset does not match the verified build: kurisu-native/);
  assert.equal(fixture.commands().filter(uploadAsset).length, 1);
  assert.ok(!fixture.commands().some(args => args[2] === "PATCH"));
});

test("retry verifies a published release after a lost response without writing", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const first = fixture.execute({ KURISU_TEST_LOSE_PUBLISH_RESPONSE: "1" });
  assert.equal(first.status, 1);
  assert.match(first.stderr, /lost publish response/);
  const draftPath = join(fixture.directory, "draft.json");
  const original = readFileSync(draftPath, "utf8");
  assert.equal(JSON.parse(original).draft, false);

  const offset = fixture.commands().length;
  fixture.restage();
  const retry = fixture.execute();
  assert.equal(retry.status, 0, retry.stderr);
  assert.match(retry.stdout, /Verified previously published release/);
  assert.ok(!fixture.commands().slice(offset).some(releaseWrite));
  assert.equal(readFileSync(draftPath, "utf8"), original);

  for (const [change, error, env] of [
    [release => { release.body = release.body.split("\n\n<!--")[0]; }, /expected published native release/],
    [release => { release.assets.pop(); }, /missing verified assets/],
    [release => { release.assets[0].digest = "sha256:" + "0".repeat(64); }, /does not match/],
    [release => { release.assets.push({ ...release.assets[0], name: "extra.zip" }); }, /Unexpected published asset/],
    [() => {}, /Remote release tag no longer points/, { KURISU_TEST_REMOTE_COMMIT: "b".repeat(40) }],
  ]) {
    const changed = JSON.parse(original);
    change(changed);
    writeFileSync(draftPath, JSON.stringify(changed));
    const result = fixture.execute(env);
    assert.equal(result.status, 1);
    assert.match(result.stderr, error);
  }
  assert.ok(!fixture.commands().slice(offset).some(releaseWrite));
});

test("partial publication rejects a rebuilt missing asset and resumes with original artifacts", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  assert.equal(fixture.execute({ KURISU_TEST_FAIL_UPLOAD: "1" }).status, 1);
  const offset = fixture.commands().length;
  const name = "Kurisu_1.1.0_linux-x64.tar.gz";
  const archive = join(fixture.stage, name);
  const sidecar = `${archive}.sha256`;
  const original = readFileSync(archive);
  const checksum = readFileSync(sidecar);
  const rebuilt = Buffer.from("rebuilt archive");
  writeFileSync(archive, rebuilt);
  writeFileSync(sidecar, `${createHash("sha256").update(rebuilt).digest("hex")}  ${name}\n`);
  const refused = fixture.execute();
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, /original verified build.*re-run failed jobs/);
  assert.ok(!fixture.commands().slice(offset).some(releaseWrite));

  fixture.restage();
  assert.deepEqual(readFileSync(archive), original);
  assert.deepEqual(readFileSync(sidecar), checksum);
  const retry = fixture.execute();
  assert.equal(retry.status, 0, retry.stderr);
  assert.equal(fixture.commands().filter(createDraft).length, 1);
});

test("retry after retention fails reuses the original artifacts and prunes remaining older releases", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const first = fixture.execute();
  assert.equal(first.status, 0, first.stderr);
  const release = JSON.parse(readFileSync(join(fixture.directory, "draft.json")));
  const older = ["v1.0.0.79", "v1.0.0.80"].map((tag_name, index) => ({
    id: index + 2, tag_name, draft: false, prerelease: false,
    assets: [...bridgeNames, ...nativeNames(tag_name)].map(name => ({ name })),
  }));
  let releases = [release, { ...bridge(), id: 1 }, ...older];
  const deleted = [];
  const runRetention = () => pruneNativeReleases("v1.1.0", fixture.stage, args => {
    if (args.includes("--paginate")) return releases.map(item => JSON.stringify(item)).join("\n");
    if (args[1] === "download") {
      const source = args[2] === transition.bridgeTag ? fixture.source : fixture.published;
      const destination = args[args.indexOf("--dir") + 1];
      for (let index = 0; index < args.length; index++) if (args[index] === "--pattern") {
        copyFileSync(join(source, args[index + 1]), join(destination, args[index + 1]));
      }
      return "";
    }
    assert.equal(args[2], "DELETE");
    const id = Number(args[3].split("/").at(-1));
    assert.ok(older.some(item => item.id === id));
    deleted.push(id);
    releases = releases.filter(item => item.id !== id);
    if (deleted.length === 1) throw new Error("lost retention response");
    return "";
  });
  assert.throws(runRetention, /lost retention response/);

  const offset = fixture.commands().length;
  fixture.restage();
  const retry = fixture.execute();
  assert.equal(retry.status, 0, retry.stderr);
  assert.ok(!fixture.commands().slice(offset).some(releaseWrite));
  assert.deepEqual(runRetention().removed, [older[1].tag_name]);
  assert.deepEqual(deleted, older.map(item => item.id));
  assert.deepEqual(releases.map(item => item.tag_name), ["v1.1.0", transition.bridgeTag]);
});

test("retry refuses changed draft metadata, assets, published state and moved tags before writing", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const first = fixture.execute({ KURISU_TEST_FAIL_UPLOAD: "1" });
  assert.equal(first.status, 1);
  const draftPath = join(fixture.directory, "draft.json");
  const original = readFileSync(draftPath, "utf8");
  for (const [change, message, env] of [
    [draft => { draft.body = "A maintainer's draft"; }, /expected native draft/],
    [draft => { draft.body = draft.body.split("\n\n<!--")[0] + `\n\n<!-- Kurisu native release draft for ${sourceCommit} -->`; }, /expected native draft/],
    [draft => { draft.upload_url = "https://other.example/assets{?name,label}"; }, /Unexpected release asset upload URL/],
    [draft => { draft.draft = false; }, /Published release is missing/],
    [draft => { draft.assets[0].digest = "sha256:" + "0".repeat(64); }, /does not match/],
    [draft => { draft.assets[0].name = "unexpected.zip"; }, /Unexpected draft asset/],
    [() => {}, /Remote release tag no longer points/, { KURISU_TEST_REMOTE_COMMIT: "b".repeat(40) }],
  ]) {
    const changed = JSON.parse(original);
    change(changed);
    const before = JSON.stringify(changed);
    writeFileSync(draftPath, before);
    const offset = fixture.commands().length;
    const retry = fixture.execute(env);
    assert.equal(retry.status, 1);
    assert.match(retry.stderr, message);
    assert.ok(!fixture.commands().slice(offset).some(releaseWrite));
    assert.equal(readFileSync(draftPath, "utf8"), before);
  }
});

test("plan exports the version and verified commit", { skip: process.platform === "win32" }, context => {
  const fixture = publicationFixture(context);
  const output = join(fixture.directory, "output");
  const result = fixture.execute({ GITHUB_OUTPUT: output }, "plan");
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(readFileSync(output, "utf8").trim().split("\n").sort(), [
    `commit=${sourceCommit}`, "tag=v1.1.0", "ver=1.1.0",
  ]);
});

test("plan rejects invalid tags before invoking git and requires HEAD to match the tag", {
  skip: process.platform === "win32",
}, context => {
  for (const badTag of ["v1.1.0-beta", transition.bridgeTag, "--verify"]) {
    const fixture = publicationFixture(context);
    const result = fixture.execute({}, "plan", badTag);
    assert.equal(result.status, 1, `plan accepted invalid tag ${badTag}`);
    assert.throws(() => readFileSync(fixture.gitLog), { code: "ENOENT" });
  }
  const mismatch = publicationFixture(context);
  const result = mismatch.execute({ KURISU_TEST_HEAD_COMMIT: "f".repeat(40) }, "plan");
  assert.equal(result.status, 1, "plan accepted a checkout that differed from the tag commit");
  assert.match(result.stderr, /Build the exact tagged source commit/);
});

test("publication rejects a tag and checkout that moved together after planning", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context, "a".repeat(40));
  const result = fixture.execute({ RELEASE_COMMIT: sourceCommit });
  assert.equal(result.status, 1, "publication accepted a commit that differed from the plan");
  assert.match(result.stderr, /Release tag no longer points to the planned commit/);
  assert.throws(() => readFileSync(fixture.log), { code: "ENOENT" });
});

test("publication rejects an invalid planned commit", { skip: process.platform === "win32" }, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute({ RELEASE_COMMIT: sourceCommit.slice(0, 12) });
  assert.equal(result.status, 1, "publication accepted an abbreviated planned commit");
  assert.match(result.stderr, /Invalid planned release commit/);
});

test("publication handles a large release history and copies verified bridge assets into the release", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const drafts = Array.from({ length: 100 }, (_, index) => ({
    tag_name: `v2.0.${index}`, draft: true, prerelease: false, body: "x".repeat(20_000),
  }));
  writeFileSync(join(fixture.directory, "releases.json"), JSON.stringify([drafts, [bridge()]]));
  const result = fixture.execute({ RELEASE_COMMIT: sourceCommit, KURISU_TEST_ANNOTATED_TAG: "1" });
  assert.equal(result.status, 0, result.stderr);
  const published = JSON.parse(readFileSync(join(fixture.directory, "draft.json")));
  assert.equal(published.target_commitish, "main");
  assert.equal(published.draft, false);
  assert.equal(published.assets.length, nativeNames("v1.1.0").length + bridgeNames.length);
  for (const { name, state, size, digest } of published.assets) {
    const local = readFileSync(join(fixture.stage, name));
    assert.equal(state, "uploaded");
    assert.equal(size, local.length);
    assert.equal(digest, `sha256:${createHash("sha256").update(local).digest("hex")}`);
  }
  for (const name of bridgeNames) {
    assert.deepEqual(readFileSync(join(fixture.stage, name)), readFileSync(join(fixture.source, name)));
  }
  const commands = fixture.commands();
  assert.ok(commands.some(args => args.includes("--paginate") && args.includes("--jq")));
  assert.equal(commands.filter(createDraft).length, 1);
  const draft = commands.find(createDraft);
  assert.equal(commands[commands.indexOf(draft) - 1][1], "repos/CateDesu/Kurisu/git/tags/" + "c".repeat(40));
  assert.ok(draft.includes(`target_commitish=${sourceCommit}`));
  const uploads = commands.filter(uploadAsset);
  assert.ok(uploads.some(args => args.includes(join(fixture.stage, installer))));
  assert.ok(uploads.some(args => args.includes(join(fixture.stage, "Kurisu-Native_1.1.0_x64-install.exe"))));
  assert.ok(uploads.every(args => !args.includes("--clobber")));
  const publish = commands.filter(releaseWrite).at(-1);
  assert.equal(publish[2], "PATCH");
  assert.equal(publish[3], "repos/CateDesu/Kurisu/releases/123");
  assert.ok(publish.includes("draft=false"));
  assert.ok(publish.includes("make_latest=true"));
});

test("damaged frozen bridge refuses publication without creating or editing a release", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  writeFileSync(join(fixture.source, "kurisu"), "damaged after original checksum was written");
  const result = fixture.execute();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /checksum did NOT match|FAILED/);
  assert.ok(!fixture.commands().some(releaseWrite));
});

test("publication preserves mismatched staged bridge bytes and retries with matching bytes", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const asset = join(fixture.stage, "kurisu");
  const checksum = asset + ".sha256";
  const mismatched = Buffer.from("different staged bridge binary");
  const sidecar = `${createHash("sha256").update(mismatched).digest("hex")}  kurisu\n`;
  writeFileSync(asset, mismatched);
  writeFileSync(checksum, sidecar);
  const refused = fixture.execute();
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, /Staged asset differs from the pinned bridge: kurisu/);
  assert.deepEqual(readFileSync(asset), mismatched);
  assert.equal(readFileSync(checksum, "utf8"), sidecar);
  assert.ok(!fixture.commands().some(releaseWrite));

  for (const name of ["kurisu", "kurisu.sha256"]) {
    writeFileSync(join(fixture.stage, name), readFileSync(join(fixture.source, name)));
  }
  const retry = fixture.execute();
  assert.equal(retry.status, 0, retry.stderr);
  assert.equal(fixture.commands().filter(createDraft).length, 1);
});

test("a draft or missing bridge asset refuses native publication before any write", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  writeFileSync(join(fixture.directory, "bridge.json"), JSON.stringify({ ...bridge(), draft: true }));
  const result = fixture.execute();
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /published stable release/);
  assert.ok(!fixture.commands().some(args => releaseWrite(args) || args[1] === "download"));
});

test("publication rejects newer stable releases and existing draft tags on later pages", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const page1 = Array.from({ length: 100 }, (_, index) => ({ tag_name: `v1.0.0.${index + 1}`, draft: true, prerelease: false }));
  for (const [tag_name, draft, expected] of [
    ["v1.2.0", false, /higher version is already published/],
    ["v1.1.0", true, /Release already exists/],
  ]) {
    writeFileSync(join(fixture.directory, "releases.json"), JSON.stringify([page1, [{ tag_name, draft, prerelease: false }]]));
    const blocked = fixture.execute({ GITHUB_REPOSITORY: "StagingOwner/Kurisu" });
    assert.notEqual(blocked.status, 0);
    assert.match(blocked.stderr, expected);
  }
  assert.ok(fixture.commands().some(args => args.includes("repos/StagingOwner/Kurisu/releases?per_page=100")));
  assert.ok(!fixture.commands().some(releaseWrite));
});

test("publication rejects uppercase checksum sidecars", { skip: process.platform === "win32" }, context => {
  const fixture = publicationFixture(context);
  const upper = readFileSync(join(fixture.stage, "kurisu-native.sha256"), "utf8").replace(/^[0-9a-f]{64}/, match => match.toUpperCase());
  writeFileSync(join(fixture.stage, "kurisu-native.sha256"), upper);
  const badHex = fixture.execute();
  assert.notEqual(badHex.status, 0);
  assert.match(badHex.stderr, /Invalid existing checksum sidecar: kurisu-native/);
});

test("publication rejects an empty native bundle with a matching checksum", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const name = "Kurisu_1.1.0_linux-x64.tar.gz";
  const empty = Buffer.alloc(0);
  writeFileSync(join(fixture.stage, name), empty);
  writeFileSync(join(fixture.stage, `${name}.sha256`), `${createHash("sha256").update(empty).digest("hex")}  ${name}\n`);
  const result = fixture.execute();
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Empty release asset: Kurisu_1\.1\.0_linux-x64\.tar\.gz/);
  assert.ok(!fixture.commands().some(releaseWrite));
});

test("publication verifies the pinned bridge installer digest even when its sidecar matches", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const replaced = Buffer.from("tampered legacy bridge installer with matching sidecar");
  writeFileSync(join(fixture.source, installer), replaced);
  writeFileSync(join(fixture.source, `${installer}.sha256`), `${createHash("sha256").update(replaced).digest("hex")}  ${installer}\n`);
  const tampered = fixture.execute();
  assert.notEqual(tampered.status, 0);
  assert.match(tampered.stderr, /Bridge installer SHA-256/);
  assert.ok(!fixture.commands().some(releaseWrite));
});

test("publication rejects a remote tag that moved after planning", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute({
    RELEASE_COMMIT: sourceCommit,
    KURISU_TEST_REMOTE_COMMIT: "b".repeat(40),
  });
  assert.equal(result.status, 1, "publication accepted a moved remote tag");
  assert.match(result.stderr, /Remote release tag no longer points to the planned commit/);
  assert.ok(!fixture.commands().some(releaseWrite));
});

test("a remote tag that moves during upload leaves the release in draft", {
  skip: process.platform === "win32",
}, context => {
  const fixture = publicationFixture(context);
  const result = fixture.execute({ RELEASE_COMMIT: sourceCommit, KURISU_TEST_COMMIT_AFTER_UPLOAD: "b".repeat(40) });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Remote release tag no longer points to the planned commit/);
  assert.ok(fixture.commands().some(uploadAsset));
  assert.ok(!fixture.commands().some(args => args[2] === "PATCH"));
});
