import { expect, test } from "@playwright/test";

test("home renders the shell", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: /overseer/ })).toBeVisible();
  await expect(page.getByRole("button", { name: "New session" })).toBeVisible();
});
