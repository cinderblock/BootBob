import { describe, expect, it } from "vitest";
import { ago, fullTime } from "./time";

describe("ago", () => {
  const now = 1_800_000_000_000;
  it("reads naturally", () => {
    expect(ago(now - 1000, now)).toBe("just now");
    expect(ago(now - 12_000, now)).toBe("12 s ago");
    expect(ago(now - 5 * 60_000, now)).toBe("5 min ago");
    expect(ago(now - 3 * 3_600_000, now)).toBe("3 h ago");
    expect(ago(now - 72 * 3_600_000, now)).toBe("3 days ago");
    expect(ago(now + 5000, now)).toBe("just now");
  });
});

describe("fullTime", () => {
  it("puts UTC after the local time", () => {
    expect(fullTime(Date.UTC(2026, 9, 23, 22, 36, 5))).toMatch(/ \(22:36:05Z\)$/);
  });
});
