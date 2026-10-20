import source from "../src-tauri/src/anilist.rs?raw";
import { expect, test } from "vitest";

const literal = source.match(/const OK_HTML: &str = ("(?:\\.|[^"\\])*");/)?.[1];
const html = JSON.parse(literal!);
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];

test("the callback page removes the authorization code from the browser address", () => {
  let replacement: unknown[] | undefined;
  new Function("location", "history", script)(
    { pathname: "/", search: "?code=example&state=expected" },
    { replaceState: (...args: unknown[]) => { replacement = args; } },
  );
  expect(replacement).toEqual([null, "", "/"]);
});
