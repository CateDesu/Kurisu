import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export function nextRollingVersion(base, count, releases) {
  if (!/^\d+\.\d+\.\d+$/.test(base) || !/^\d+$/.test(String(count))) {
    throw new Error("Invalid release version or commit count");
  }
  const core = base.split(".").map(BigInt);
  let next = BigInt(count);
  for (const release of releases) {
    if (release.isPrerelease) continue;
    const match = /^v?(\d+)\.(\d+)\.(\d+)(?:\.(\d+))?$/.exec(release.tagName);
    if (!match) continue;
    const published = match.slice(1, 4).map(BigInt);
    const difference = published.findIndex((n, i) => n !== core[i]);
    if (difference !== -1) {
      if (!release.isDraft && published[difference] > core[difference]) {
        throw new Error(`Update the base version beyond ${release.tagName} before publishing`);
      }
      continue;
    }
    if (match[4] !== undefined && BigInt(match[4]) >= next) {
      next = BigInt(match[4]) + 1n;
    }
  }
  return `v${base}.${next}`;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const config = JSON.parse(readFileSync("src-tauri/tauri.conf.json", "utf8"));
  const releases = JSON.parse(readFileSync(0, "utf8"));
  process.stdout.write(nextRollingVersion(config.version, process.argv[2], releases));
}
