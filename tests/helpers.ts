import { flushSync, tick } from "svelte";

export function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

export async function settle() {
  for (let i = 0; i < 8; i++) await Promise.resolve();
  flushSync();
  await tick();
}

export function button(text: string, scope: ParentNode = document): HTMLButtonElement {
  const result = [...scope.querySelectorAll("button")].find((item) => item.textContent?.trim() === text);
  if (!result) throw new Error(`Missing button: ${text}`);
  return result;
}

export function selectOption(selector: string, value: string) {
  const element = document.querySelector<HTMLSelectElement>(selector);
  if (!element) throw new Error(`Missing select: ${selector}`);
  element.value = value;
  element.dispatchEvent(new Event("change", { bubbles: true }));
}
