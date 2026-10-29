export type ScrollPosition = { left: number; top: number };

export function rememberScroll(node: HTMLElement) {
  let stopRestoring: (() => void) | undefined;
  let releaseAnchor: (() => void) | undefined;

  function stopWaiting() {
    stopRestoring?.();
    stopRestoring = undefined;
  }

  function stop() {
    stopWaiting();
    releaseAnchor?.();
  }

  function capture(): ScrollPosition {
    stop();
    return { left: node.scrollLeft, top: node.scrollTop };
  }

  function restore(position: ScrollPosition) {
    stop();
    const anchor = node.style.getPropertyValue("overflow-anchor");
    const priority = node.style.getPropertyPriority("overflow-anchor");
    let frame = 0;
    node.style.setProperty("overflow-anchor", "none", priority);
    const release = () => {
      cancelAnimationFrame(frame);
      if (anchor) node.style.setProperty("overflow-anchor", anchor, priority);
      else node.style.removeProperty("overflow-anchor");
      if (releaseAnchor === release) releaseAnchor = undefined;
    };
    releaseAnchor = release;
    const complete = () => {
      stopWaiting();
      // Let restored content paint before browser anchoring resumes.
      frame = requestAnimationFrame(() => { frame = requestAnimationFrame(release); });
    };
    const apply = () => {
      if (node.querySelector("[data-pending-rows]")) return false;
      node.scrollLeft = position.left;
      node.scrollTop = position.top;
      return Math.abs(node.scrollLeft - position.left) < 1 &&
        Math.abs(node.scrollTop - position.top) < 1;
    };
    if (apply()) {
      complete();
      return;
    }

    // Cached rows can arrive after navigation finishes.
    let waiting = true;
    const retry = () => { if (waiting && apply()) complete(); };
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
    mutations.observe(node, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ["data-pending-rows"] });
    sizes.observe(node);
    observeChildren();
    node.addEventListener("wheel", stop, { passive: true });
    node.addEventListener("pointerdown", stop, { passive: true });
    node.addEventListener("touchstart", stop, { passive: true });
    window.addEventListener("keydown", stop);
    stopRestoring = () => {
      waiting = false;
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
