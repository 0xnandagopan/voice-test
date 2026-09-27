import { test, expect } from "@playwright/test";
// Opt-in real API test. The operator credentials come from process environment; never include secrets in reports.
test("real API invitation, consent, reload and revocation", async ({
  browser,
  page,
}) => {
  test.skip(
    !process.env.TEST_OPERATOR_USERNAME || !process.env.TEST_OPERATOR_PASSWORD,
    "Set TEST_OPERATOR_USERNAME and TEST_OPERATOR_PASSWORD for a running local API.",
  );
  await page.goto("/operator");
  await page
    .getByLabel("Username", { exact: true })
    .fill(process.env.TEST_OPERATOR_USERNAME!);
  await page
    .getByLabel("Password", { exact: true })
    .fill(process.env.TEST_OPERATOR_PASSWORD!);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  const label = `Synthetic browser ${Date.now()}`;
  await page.getByLabel("Customer name").fill(label);
  await page
    .getByLabel("Project context")
    .fill("Synthetic local integration test; no personal data.");
  await page.getByRole("button", { name: "Create private invitation" }).click();
  const url = await page.getByLabel("Private invitation link").inputValue();
  const customer = await browser.newContext();
  const customerPage = await customer.newPage();
  await customerPage.goto(url);
  await expect(
    customerPage.getByRole("heading", { name: "Your voice, your choice." }),
  ).toBeVisible();
  expect(new URL(customerPage.url()).hash).toBe("");
  await customerPage.getByRole("checkbox", { name: /I consent/ }).check();
  await customerPage
    .getByRole("button", { name: "I agree — check my sound" })
    .click();
  await expect(
    customerPage.getByRole("heading", { name: "Let’s check your sound." }),
  ).toBeVisible();
  await customerPage.reload();
  await expect(
    customerPage.getByRole("heading", { name: "Let’s check your sound." }),
  ).toBeVisible();
  await page.reload();
  page.once("dialog", (dialog) => dialog.accept());
  await page
    .getByRole("listitem")
    .filter({ hasText: label })
    .getByRole("button", { name: "Revoke", exact: true })
    .click();
  await expect(
    page
      .getByRole("listitem")
      .filter({ hasText: label })
      .getByText("revoked", { exact: true }),
  ).toBeVisible();
  await customerPage.reload();
  await expect(
    customerPage.getByRole("heading", {
      name: "We couldn’t open this invitation.",
    }),
  ).toBeVisible();
  await customer.close();
});
