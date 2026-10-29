const rowOwners = new WeakMap<HTMLElement, (row: HTMLElement) => void>();

export function measuredRows(node: HTMLElement) {
  if (typeof ResizeObserver === "undefined" || typeof CSS === "undefined" ||
      !CSS.supports("content-visibility", "auto") ||
      !CSS.supports("contain-intrinsic-block-size", "1px")) return;

  type Row = {
    width: number;
    height: number;
    retained: boolean;
    originalVisibility: string;
    originalSize: string;
    writtenStyle: string | null;
  };
  const rows = new Map<HTMLElement, Row>();
  const invalid = new Set<HTMLElement>();
  const measured = new Map<HTMLElement, { width: number; height: number }>();
  const nearby = new Set<HTMLElement>();
  let width = 0;
  let frame = 0;
  let destroyed = false;

  function write(row: HTMLElement, visibility: string, size: string) {
    if (row.style.getPropertyValue("content-visibility") !== visibility) {
      row.style.setProperty("content-visibility", visibility);
    }
    const previous = row.style.getPropertyValue("contain-intrinsic-block-size");
    if (previous !== size) {
      if (size) row.style.setProperty("contain-intrinsic-block-size", size);
      else row.style.removeProperty("contain-intrinsic-block-size");
    }
    rows.get(row)!.writtenStyle = row.getAttribute("style");
  }

  function schedule() {
    if (!frame && !destroyed) frame = requestAnimationFrame(apply);
  }

  function invalidate(row: HTMLElement) {
    if (rows.get(row)?.retained) return;
    invalid.add(row);
    measured.delete(row);
    schedule();
  }

  function invalidateAll() {
    for (const row of rows.keys()) invalidate(row);
  }

  function apply() {
    frame = 0;
    if (destroyed) return;
    for (const row of nearby) {
      if (rows.has(row)) write(row, "visible", "");
    }
    nearby.clear();
    for (const row of invalid) {
      const state = rows.get(row);
      if (!state) continue;
      state.width = 0;
      write(row, "visible", "");
      // A fresh observation also reports rows whose size stayed the same.
      sizes.unobserve(row);
      sizes.observe(row);
    }
    invalid.clear();
    for (const [row, size] of measured) {
      const state = rows.get(row);
      if (!state) continue;
      state.width = size.width;
      state.height = size.height;
      if (row.contains(node.ownerDocument.activeElement)) continue;
      write(row, "auto", `${size.height}px`);
    }
    measured.clear();
  }

  const sizes = new ResizeObserver((entries) => {
    if (destroyed) return;
    for (const entry of entries) {
      const { width: nextWidth, height } = entry.contentRect;
      if (entry.target === node) {
        if (width && nextWidth !== width) invalidateAll();
        width = nextWidth;
        continue;
      }
      const row = entry.target as HTMLElement;
      const state = rows.get(row);
      if (!state || state.retained || invalid.has(row)) continue;
      if (state.width && state.width !== nextWidth) {
        invalidate(row);
      } else if (nextWidth > 0 && height > 0) {
        if (state.width === nextWidth && state.height === height) {
          measured.delete(row);
          continue;
        }
        measured.set(row, { width: nextWidth, height });
        schedule();
      }
    }
  });

  // Prepare rows before they enter view and retain layout for return scrolling.
  const proximity = typeof IntersectionObserver === "undefined" ? undefined : new IntersectionObserver((entries) => {
    if (destroyed) return;
    for (const entry of entries) {
      const row = entry.target as HTMLElement;
      const state = rows.get(row);
      if (!state || state.retained || !entry.isIntersecting) continue;
      state.retained = true;
      // Retained rows use natural layout and no longer need measurements.
      invalid.delete(row);
      measured.delete(row);
      sizes.unobserve(row);
      nearby.add(row);
      proximity?.unobserve(row);
      schedule();
    }
  }, { root: node.closest("main"), rootMargin: "800px 0px" });

  function restore(row: HTMLElement, state: Row) {
    if (state.originalVisibility) row.style.setProperty("content-visibility", state.originalVisibility);
    else row.style.removeProperty("content-visibility");
    if (state.originalSize) row.style.setProperty("contain-intrinsic-block-size", state.originalSize);
    else row.style.removeProperty("contain-intrinsic-block-size");
  }

  function unregister(row: HTMLElement) {
    const state = rows.get(row);
    if (!state) return;
    sizes.unobserve(row);
    proximity?.unobserve(row);
    invalid.delete(row);
    measured.delete(row);
    nearby.delete(row);
    rows.delete(row);
    rowOwners.delete(row);
    restore(row, state);
  }

  function register(row: HTMLElement) {
    if (rows.has(row)) return;
    // Restore the previous list's styles before adopting a moved row.
    rowOwners.get(row)?.(row);
    rows.set(row, {
      width: 0,
      height: 0,
      retained: false,
      originalVisibility: row.style.getPropertyValue("content-visibility"),
      originalSize: row.style.getPropertyValue("contain-intrinsic-block-size"),
      writtenStyle: row.getAttribute("style"),
    });
    rowOwners.set(row, unregister);
    sizes.observe(row);
    proximity?.observe(row);
  }

  function containingRow(target: Node | null) {
    let row = target instanceof HTMLElement ? target : target?.parentElement;
    while (row && row.parentElement !== node) row = row.parentElement;
    return row && rows.has(row) ? row : undefined;
  }

  const changes = new MutationObserver((records) => {
    if (destroyed) return;
    const changedRows = new Set<HTMLElement>();
    for (const record of records) {
      if (record.target !== node || record.type !== "childList") continue;
      for (const children of [record.addedNodes, record.removedNodes]) {
        for (const child of children) {
          if (child instanceof HTMLElement) changedRows.add(child);
        }
      }
    }
    for (const row of changedRows) {
      if (row.parentElement === node) register(row);
      else unregister(row);
    }
    for (const record of records) {
      if (record.target === node) {
        if (record.type === "attributes") invalidateAll();
        continue;
      }
      const row = containingRow(record.target);
      if (!row) continue;
      if (record.target === row && record.attributeName === "style") {
        const state = rows.get(row)!;
        if (row.getAttribute("style") === state.writtenStyle) continue;
        if (state.retained) {
          nearby.add(row);
          schedule();
          continue;
        }
      }
      invalidate(row);
    }
  });

  function focus(event: FocusEvent) {
    const row = containingRow(event.target as Node);
    if (!row) return;
    if (event.type === "focusin") write(row, "visible", "");
    invalidate(row);
  }

  function loaded(event: Event) {
    // Marked cover wrappers reserve their image dimensions before loading.
    if (event.target instanceof HTMLImageElement &&
        event.target.parentElement?.hasAttribute("data-measured-cover")) return;
    const row = containingRow(event.target as Node);
    if (row) invalidate(row);
  }

  for (const child of node.children) {
    if (child instanceof HTMLElement) register(child);
  }
  sizes.observe(node);
  changes.observe(node, { childList: true, subtree: true, characterData: true, attributes: true });
  node.addEventListener("focusin", focus);
  node.addEventListener("focusout", focus);
  node.addEventListener("load", loaded, true);
  node.ownerDocument.fonts?.addEventListener("loadingdone", invalidateAll);

  return {
    destroy() {
      destroyed = true;
      cancelAnimationFrame(frame);
      changes.disconnect();
      sizes.disconnect();
      proximity?.disconnect();
      node.removeEventListener("focusin", focus);
      node.removeEventListener("focusout", focus);
      node.removeEventListener("load", loaded, true);
      node.ownerDocument.fonts?.removeEventListener("loadingdone", invalidateAll);
      for (const [row, state] of rows) {
        rowOwners.delete(row);
        restore(row, state);
      }
      rows.clear();
      invalid.clear();
      measured.clear();
      nearby.clear();
    },
  };
}
