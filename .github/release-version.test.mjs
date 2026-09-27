import assert from "node:assert/strict";
import { test } from "node:test";
import { nextRollingVersion } from "./release-version.mjs";

test("first release uses the commit count", () => {
  assert.equal(nextRollingVersion("1.0.0", 52, []), "v1.0.0.52");
});

test("amended and squashed histories advance the published version", () => {
  const releases = [{ tagName: "v1.0.0.52" }];
  assert.equal(nextRollingVersion("1.0.0", 52, releases), "v1.0.0.53");
  assert.equal(nextRollingVersion("1.0.0", 49, releases), "v1.0.0.53");
  assert.equal(nextRollingVersion("1.0.0", 60, releases), "v1.0.0.60");
});

test("release order and unpublished higher bases do not change the next version", () => {
  const releases = [
    { tagName: "v1.0.0.60" },
    { tagName: "v1.0.0.52" },
    { tagName: "v9.0.0.99", isDraft: true },
    { tagName: "v9.0.0.99", isPrerelease: true },
    { tagName: "rolling" },
  ];
  assert.equal(nextRollingVersion("1.0.0", 52, releases), "v1.0.0.61");
  assert.equal(nextRollingVersion("1.0.0", 52, releases.reverse()), "v1.0.0.61");
});

test("canceled builds reserve their draft version for the old commit", () => {
  const releases = [
    { tagName: "v1.0.0.52" },
    { tagName: "v1.0.0.53", isDraft: true },
  ];
  assert.equal(nextRollingVersion("1.0.0", 53, releases), "v1.0.0.54");
});

test("base version bumps start their own rolling counter", () => {
  assert.equal(nextRollingVersion("1.1.0", 53, [{ tagName: "v1.0.0.99" }]), "v1.1.0.53");
  assert.equal(nextRollingVersion("1.1.0", 53, [{ tagName: "v1.1.0" }]), "v1.1.0.53");
});

test("older bases and malformed inputs cannot publish", () => {
  assert.throws(() => nextRollingVersion("1.0.0", 53, [{ tagName: "v1.1.0" }]));
  assert.throws(() => nextRollingVersion("1.0", 53, []));
  assert.throws(() => nextRollingVersion("1.0.0", -1, []));
});
