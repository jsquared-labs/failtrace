const missing = require("./this-module-does-not-exist");

test("uses missing module", () => {
  expect(missing.value).toBe(1);
});
