import "@testing-library/jest-dom";
import { configure } from "@testing-library/react";
import { beforeAll, afterEach, afterAll } from "vitest";
import { server } from "./msw-server";

// jsdom has no matchMedia; Radix/container-query components crash without a stub.
if (!window.matchMedia) {
  window.matchMedia = (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  }) as unknown as MediaQueryList;
}

// jsdom has no ResizeObserver; react-grid-layout's useContainerWidth needs a stub.
if (!window.ResizeObserver) {
  window.ResizeObserver = class ResizeObserver {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}

// Default 1s async-util budget flakes when the machine is CPU-starved.
configure({ asyncUtilTimeout: 5000 });

beforeAll(() => server.listen({ onUnhandledRequest: "warn" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

// jsdom's AbortSignal comes from a different realm than Node's fetch, so undici rejects it inside
// MSW; reimplement abort semantics in JS. Registered after server.listen() so it wraps MSW's fetch.
beforeAll(() => {
  const mswFetch = globalThis.fetch;
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    const { signal, ...rest } = init ?? {};
    if (!signal) return mswFetch(input, init);
    return new Promise<Response>((resolve, reject) => {
      const abortNow = () => reject(new DOMException("The operation was aborted.", "AbortError"));
      if (signal.aborted) {
        abortNow();
        return;
      }
      signal.addEventListener("abort", abortNow, { once: true });
      mswFetch(input, rest).then(
        (res) => {
          signal.removeEventListener("abort", abortNow);
          resolve(res);
        },
        (err) => {
          signal.removeEventListener("abort", abortNow);
          reject(err);
        },
      );
    });
  }) as typeof fetch;
});

// Same realm split for `new Request()` (react-router builds one per navigation): undici rejects a
// jsdom signal, so retry without it. Tests never rely on that Request's `.signal`.
beforeAll(() => {
  const mswRequest = globalThis.Request;
  globalThis.Request = new Proxy(mswRequest, {
    construct(target, args, newTarget) {
      try {
        return Reflect.construct(target, args, newTarget);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        if (!/AbortSignal|Expected signal/.test(message)) throw err;
        const [input, init] = args as [RequestInfo | URL, RequestInit | undefined];
        const { signal: _signal, ...rest } = init ?? {};
        return Reflect.construct(target, [input, rest], newTarget);
      }
    },
  });
});

// jsdom lacks these; Radix Select/Popover call them on pointerdown.
if (!Element.prototype.hasPointerCapture) {
  Element.prototype.hasPointerCapture = () => false;
}
if (!Element.prototype.setPointerCapture) {
  Element.prototype.setPointerCapture = () => {};
}
if (!Element.prototype.releasePointerCapture) {
  Element.prototype.releasePointerCapture = () => {};
}
if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}
