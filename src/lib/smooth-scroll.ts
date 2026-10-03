export function smoothScroll(node: HTMLElement, _route?: string) {
  const reducedMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)");
  let frame = 0;
  let active: HTMLElement | null = null;
  let target = 0;
  let position = 0;
  let velocity = 0;
  let lastTop = 0;
  let lastTime = 0;
  let preciseUntil = 0;

  function stop() {
    if (frame) cancelAnimationFrame(frame);
    frame = 0;
    active = null;
    velocity = 0;
  }

  function advance(now: number) {
    const elapsed = Math.max(0, now - lastTime);
    const frequency = 1 / 26;
    const offset = position - target;
    const decay = Math.exp(-frequency * elapsed);
    const momentum = velocity + frequency * offset;
    // Critical damping keeps velocity continuous without a bounce.
    position = target + (offset + momentum * elapsed) * decay;
    velocity = (velocity - frequency * momentum * elapsed) * decay;
    lastTime += elapsed;
  }

  function animate(now: number) {
    frame = 0;
    if (!active || !active.isConnected || Math.abs(active.scrollTop - lastTop) > 1) {
      stop();
      return;
    }
    const limit = Math.max(0, active.scrollHeight - active.clientHeight);
    target = Math.max(0, Math.min(target, limit));
    advance(now);
    if (position < 0 || position > limit) {
      position = Math.max(0, Math.min(position, limit));
      velocity = 0;
    }
    if (Math.abs(target - position) < 0.25 && Math.abs(velocity) < 0.01) {
      active.scrollTop = target;
      stop();
    } else {
      // Keep fractional motion even when the webview rounds scrollTop.
      active.scrollTop = position;
      lastTop = active.scrollTop;
      frame = requestAnimationFrame(animate);
    }
  }

  function scrollTarget(event: WheelEvent): HTMLElement | null {
    for (const element of event.composedPath()) {
      if (!(element instanceof HTMLElement)) continue;
      if (element.matches('textarea, select, input[type="number"], input[type="range"], [contenteditable]:not([contenteditable="false"])')) return null;
      const style = getComputedStyle(element);
      if (/^(auto|scroll)$/.test(style.overflowY)) {
        const canScroll = event.deltaY < 0
          ? element.scrollTop > 0
          : element.scrollTop < element.scrollHeight - element.clientHeight;
        if (canScroll) return element;
        if (style.overscrollBehaviorY === "contain" || style.overscrollBehaviorY === "none") return null;
      }
      if (element === node) break;
    }
    return null;
  }

  function wheel(event: WheelEvent) {
    const mode = event.deltaMode;
    if (event.defaultPrevented || !event.cancelable || reducedMotion?.matches ||
        event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) {
      stop();
      return;
    }
    const now = performance.now();
    // Fine pixel input already carries the trackpad's motion.
    if (mode === WheelEvent.DOM_DELTA_PIXEL &&
        (event.deltaX !== 0 || Math.abs(event.deltaY) < 40 ||
          !Number.isInteger(event.deltaY) || now < preciseUntil)) {
      preciseUntil = now + 250;
      stop();
      return;
    }
    if (event.deltaX !== 0 || event.deltaY === 0) {
      stop();
      return;
    }
    const element = scrollTarget(event);
    if (!element) {
      stop();
      return;
    }
    let delta = event.deltaY;
    if (mode === WheelEvent.DOM_DELTA_LINE) {
      const style = getComputedStyle(element);
      delta *= parseFloat(style.lineHeight) || (parseFloat(style.fontSize) || 14) * 1.5;
    } else if (mode === WheelEvent.DOM_DELTA_PAGE) {
      delta *= element.clientHeight;
    }
    const current = element.scrollTop;
    if (active !== element || Math.abs(current - lastTop) > 1 ||
        Math.sign(delta) !== Math.sign(target - current)) {
      stop();
      active = element;
      position = target = current;
      lastTime = now;
    } else {
      advance(now);
    }
    target = Math.max(0, Math.min(target + delta, element.scrollHeight - element.clientHeight));
    event.preventDefault();
    lastTop = current;
    if (!frame) {
      frame = requestAnimationFrame(animate);
    }
  }

  node.addEventListener("wheel", wheel, { passive: false });
  node.addEventListener("pointerdown", stop, { passive: true });
  node.addEventListener("touchstart", stop, { passive: true });
  window.addEventListener("keydown", stop);
  window.addEventListener("blur", stop);
  reducedMotion?.addEventListener("change", stop);

  return {
    update: stop,
    destroy() {
      stop();
      node.removeEventListener("wheel", wheel);
      node.removeEventListener("pointerdown", stop);
      node.removeEventListener("touchstart", stop);
      window.removeEventListener("keydown", stop);
      window.removeEventListener("blur", stop);
      reducedMotion?.removeEventListener("change", stop);
    },
  };
}
