import { describe, expect, it } from "vitest";
import {
  getReconnectDelay,
  INITIAL_RECONNECT_DELAY,
  MAX_RECONNECT_DELAY,
  MIN_RECONNECT_DELAY,
  RECONNECT_JITTER_FACTOR,
  RECONNECT_MULTIPLIER,
} from "./useEventSource";

describe("getReconnectDelay", () => {
  it("applies bounded symmetric jitter around the capped exponential base", () => {
    const reconnectAttempt = 2;
    const baseDelay = Math.min(
      INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, reconnectAttempt),
      MAX_RECONNECT_DELAY,
    );

    expect(getReconnectDelay(reconnectAttempt, () => 0)).toBe(
      baseDelay * (1 - RECONNECT_JITTER_FACTOR),
    );
    expect(getReconnectDelay(reconnectAttempt, () => 0.5)).toBe(baseDelay);
    expect(getReconnectDelay(reconnectAttempt, () => 1)).toBe(
      baseDelay * (1 + RECONNECT_JITTER_FACTOR),
    );
  });

  it("clamps jittered delays to the documented minimum and maximum", () => {
    expect(getReconnectDelay(-20, () => 0)).toBe(MIN_RECONNECT_DELAY);

    const cappedAttempt = 10;
    const cappedBase = Math.min(
      INITIAL_RECONNECT_DELAY * Math.pow(RECONNECT_MULTIPLIER, cappedAttempt),
      MAX_RECONNECT_DELAY,
    );

    expect(cappedBase).toBe(MAX_RECONNECT_DELAY);
    expect(getReconnectDelay(cappedAttempt, () => 1)).toBe(MAX_RECONNECT_DELAY);
  });
});
