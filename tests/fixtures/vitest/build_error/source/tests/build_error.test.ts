import { describe, it, expect } from "vitest";
import { nonexistent } from "./this-module-does-not-exist";

describe("build error sample", () => {
  it("should not be reached", () => {
    expect(nonexistent()).toBe(1);
  });
});
