import { readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { createContext, runInContext } from "node:vm";
import { expect, test } from "vitest";

type Merge = ((target: unknown, source: unknown, options?: Record<string, unknown>) => Record<PropertyKey, unknown>) & {
  all: (items: unknown[], options?: Record<string, unknown>) => unknown;
};
const pluginRequire = createRequire(import.meta.resolve("@sveltejs/vite-plugin-svelte"));
const merge = pluginRequire("deepmerge") as Merge;

test("the Svelte plugin resolves the private patched fork through its deepmerge import", () => {
  expect(pluginRequire("deepmerge/package.json")).toMatchObject({
    name: "@kurisu/deepmerge", version: "4.3.1", private: true, license: "MIT",
  });
  expect(realpathSync(pluginRequire.resolve("deepmerge"))).toBe(
    realpathSync("vendor/deepmerge/index.cjs"),
  );
});

test("the patched build dependency preserves nested configuration and input ownership", () => {
  const run = () => "preprocess";
  const target = { compilerOptions: { dev: true }, preprocess: [run], files: ["first"] };
  const source = { compilerOptions: { css: "external" }, files: ["second"] };
  const result = merge(target, source, { arrayMerge: (_target: unknown[], source: unknown[]) => source });
  expect(result).toEqual({ compilerOptions: { dev: true, css: "external" }, preprocess: [run], files: ["second"] });
  expect(target.files).toEqual(["first"]);
  expect(source.compilerOptions).toEqual({ css: "external" });
  expect(Object.getPrototypeOf(result)).toBe(Object.prototype);
  expect(Object.getOwnPropertyDescriptor(result, "compilerOptions")).toMatchObject({ enumerable: true, writable: true, configurable: true });
});

test("symbols, arrays, null dictionaries and custom merging retain their contracts", () => {
  const key = Symbol("configuration");
  const dictionary = Object.assign(Object.create(null), { options: { enabled: true }, [key]: "symbol" });
  expect(merge(dictionary, { options: { verbose: true } })).toEqual({ options: { enabled: true, verbose: true }, [key]: "symbol" });
  expect(merge.all([{ files: [1] }, { files: [2] }])).toEqual({ files: [1, 2] });
  expect(merge({ options: { x: 1 } }, { options: { x: 2 } }, { customMerge: () => (_left: unknown, right: unknown) => right })).toEqual({ options: { x: 2 } });
  expect(() => merge.all(null as unknown as unknown[])).toThrow("first argument should be an array");
});

test("own dictionary fields remain writable data properties with frozen intrinsic defaults", () => {
  const module = { exports: {} as Merge };
  const context = createContext({ module });
  runInContext("Object.freeze(Object.prototype)", context);
  runInContext(readFileSync(pluginRequire.resolve("deepmerge"), "utf8"), context);
  const hardenedMerge = module.exports;
  const cases = [
    [{ toString: "target" }, {}, "target"],
    [Object.create(null), { toString: "source" }, "source"],
    [{ toString: { enabled: true } }, { toString: { verbose: true } }, { enabled: true, verbose: true }],
  ];
  for (const [target, source, value] of cases) {
    const result = hardenedMerge(target, source);
    expect(Object.getOwnPropertyDescriptor(result, "toString")).toEqual({
      value, enumerable: true, writable: true, configurable: true,
    });
    expect(Object.getPrototypeOf(result)).toBe(runInContext("Object.prototype", context));
  }
});
