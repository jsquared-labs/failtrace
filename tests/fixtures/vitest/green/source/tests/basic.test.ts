import { describe, expect, it } from "vitest";

describe("addition", () => {
  it("adds two numbers", () => {
    expect(1 + 1).toBe(2);
  });

  it("adds three numbers", () => {
    expect(1 + 2 + 3).toBe(6);
  });
});
