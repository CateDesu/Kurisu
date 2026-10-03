export type ScrollPosition = { left: number; top: number };

export function rememberScroll(node: HTMLElement) {
  let stopRestoring: (() => void) | undefined;

  function stop() {
    stopRestoring?.();
    stopRestoring = undefined;
  }

  function capture(): ScrollPosition {
    stop();
    return { left: node.scrollLeft, top: node.scrollTop };
  }

  function restore(position: ScrollPosition) {
    stop();
    const apply = () => {
      node.scrollLeft = position.left;
      node.scrollTop = position.top;
      return Math.abs(node.scrollLeft - position.left) < 1 &&
        Math.abs(node.scrollTop - position.top) < 1;
    };
    if (apply()) return;

    // Cached rows can arrive after navigation finishes.
    const retry = () => { if (apply()) stop(); };
    const sizes = new ResizeObserver(retry);
    const children = new Set<Element>();
    const observeChildren = () => {
      for (const child of children) {
        if (child.parentElement !== node) {
          sizes.unobserve(child);
          children.delete(child);
        }
      }
      for (const child of node.children) {
        if (!children.has(child)) {
          sizes.observe(child);
          children.add(child);
        }
      }
    };
    const mutations = new MutationObserver(() => { observeChildren(); retry(); });
    mutations.observe(node, { childList: true, subtree: true, characterData: true });
    sizes.observe(node);
    observeChildren();
    node.addEventListener("wheel", stop, { passive: true });
    node.addEventListener("pointerdown", stop, { passive: true });
    node.addEventListener("touchstart", stop, { passive: true });
    window.addEventListener("keydown", stop);
    stopRestoring = () => {
      mutations.disconnect();
      sizes.disconnect();
      node.removeEventListener("wheel", stop);
      node.removeEventListener("pointerdown", stop);
      node.removeEventListener("touchstart", stop);
      window.removeEventListener("keydown", stop);
    };
  }

  return {
    capture,
    restore,
    reset() {
      stop();
      node.scrollLeft = 0;
      node.scrollTop = 0;
    },
    destroy: stop,
  };
}
