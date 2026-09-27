import source from "../src-tauri/src/anilist.rs?raw";
import { expect, test } from "vitest";

const literal = source.match(/const SHIM_HTML: &str = ("(?:\\.|[^"\\])*");/)?.[1];
const html = JSON.parse(literal!);
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];

for (const fragment of ["access_token=example&state=expected", "error=access_denied&state=expected"]) {
  test(`OAuth callback forwards ${fragment.split("=")[0]}`, () => {
    let redirect: string | undefined;
    new Function("location", script)({
      hash: `#${fragment}`, replace: (url: string) => { redirect = url; },
    });
    expect(redirect).toBe(`/__capture__?${fragment}`);
  });
}
