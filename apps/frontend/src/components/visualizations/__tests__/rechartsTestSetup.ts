import { beforeAll, afterAll } from "vitest";

// ResponsiveContainer bails out at jsdom's default 0x0, and a huge `#recharts_measurement_span`
// collapses the plot area: report a real size only for the container and the span.
export function setupRechartsJsdom() {
  const realGetBoundingClientRect = HTMLElement.prototype.getBoundingClientRect;

  beforeAll(() => {
    HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
      if (this.id === "recharts_measurement_span") {
        return domRect(20, 14);
      }
      if (this.classList.contains("recharts-responsive-container")) {
        return domRect(500, 300);
      }
      return realGetBoundingClientRect.call(this);
    };
    // jsdom has no ResizeObserver; the container is measured once on mount, enough here.
    global.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof ResizeObserver;
  });

  afterAll(() => {
    HTMLElement.prototype.getBoundingClientRect = realGetBoundingClientRect;
  });
}

function domRect(width: number, height: number): DOMRect {
  return {
    width,
    height,
    top: 0,
    left: 0,
    bottom: height,
    right: width,
    x: 0,
    y: 0,
    toJSON() {
      return this;
    },
  };
}
