import { afterEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import Confirm from "$lib/Confirm.svelte";
import { button, settle } from "./helpers";

const mounted: ReturnType<typeof mount>[] = [];

afterEach(async () => {
  for (const instance of mounted.splice(0)) await unmount(instance);
  document.body.replaceChildren();
});

async function confirm(busy = false) {
  const target = document.createElement("div");
  document.body.append(target);
  mounted.push(mount(Confirm, {
    target,
    props: { title: "Remove show", busy, onconfirm: vi.fn(), oncancel: vi.fn() },
  }));
  await settle();
}

test("Tab stays inside the active confirmation", async () => {
  await confirm();
  const first = button("Cancel");
  const last = button("Confirm");
  last.focus();
  const tab = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
  last.dispatchEvent(tab);
  expect(tab.defaultPrevented).toBe(true);
  expect(document.activeElement).toBe(first);

  const back = new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true });
  first.dispatchEvent(back);
  expect(back.defaultPrevented).toBe(true);
  expect(document.activeElement).toBe(last);
});

test("only the top confirmation handles keyboard focus", async () => {
  await confirm();
  await confirm();
  const dialogs = document.querySelectorAll('[role="dialog"]');
  const first = button("Cancel", dialogs[1]);
  const last = button("Confirm", dialogs[1]);
  last.focus();
  last.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(first);
});

test("a busy confirmation keeps focus when every control is disabled", async () => {
  await confirm(true);
  const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(dialog);
});

test("closing a confirmation restores its original focused control", async () => {
  const trigger = document.createElement("button");
  document.body.append(trigger);
  trigger.focus();
  await confirm();
  await unmount(mounted.pop()!);
  expect(document.activeElement).toBe(trigger);
});

test("Shift Tab wraps from the selected radio at the start of a dialog", async () => {
  await confirm();
  const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
  const group = document.createElement("div");
  group.innerHTML = '<input type="radio" name="scope" value="folder"><input type="radio" name="scope" value="file" checked>';
  dialog.prepend(group);
  const selected = group.querySelector<HTMLInputElement>("input:checked")!;
  selected.focus();
  selected.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(button("Confirm"));
});

test("Tab wraps from a selected radio when unchecked peers follow it", async () => {
  await confirm();
  const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
  const group = document.createElement("div");
  group.innerHTML = '<input type="radio" name="scope" value="folder" checked><input type="radio" name="scope" value="file">';
  dialog.append(group);
  const selected = group.querySelector<HTMLInputElement>("input:checked")!;
  selected.focus();
  selected.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  expect(document.activeElement).toBe(button("Cancel"));
});
