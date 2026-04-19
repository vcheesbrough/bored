import { test, expect } from '@playwright/test';

// DELIBERATELY FAILING — remove once screenshot reporting is verified.
test('deliberate failure to verify screenshot capture', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('.this-element-does-not-exist')).toBeVisible();
});
