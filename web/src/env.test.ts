import { expect, test } from "vitest";
import { env } from "./env";

test("parses the Vite env at startup", () => {
  expect(env.MODE).toBe("test");
});
