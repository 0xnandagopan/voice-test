import { test, expect } from "@playwright/test";
// Opt-in real API test. The operator credentials come from process environment; never include secrets in reports.
test("real API invitation, consent, synthetic readiness, unavailable voice and revocation", async ({
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
  await customerPage.addInitScript(() => {
    const getUserMedia = navigator.mediaDevices.getUserMedia.bind(
      navigator.mediaDevices,
    );
    Object.assign(window, { micCalls: 0 });
    navigator.mediaDevices.getUserMedia = async (constraints) => {
      const state = window as unknown as { micCalls: number };
      state.micCalls++;
      return getUserMedia(constraints);
    };
  });
  await customerPage.goto(url);
  await expect(
    customerPage.getByRole("heading", { name: "Your voice, your choice." }),
  ).toBeVisible();
  expect(new URL(customerPage.url()).hash).toBe("");
  expect(
    await customerPage.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(0);
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
  expect(
    await customerPage.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(0);
  await customerPage
    .getByRole("button", { name: "Check microphone", exact: true })
    .click();
  await expect(
    customerPage.getByText("Microphone is accessible."),
  ).toBeVisible();
  await customerPage.getByRole("button", { name: "Play test tone" }).click();
  await customerPage
    .getByRole("checkbox", { name: "I heard the test tone." })
    .check();
  await customerPage
    .getByRole("button", { name: "Continue to conversation" })
    .click();
  const startResponse = customerPage.waitForResponse(
    (response) => new URL(response.url()).pathname === "/api/customer/start",
  );
  await customerPage
    .getByRole("button", { name: "Start interview", exact: true })
    .click();
  const unavailable = await startResponse;
  expect(unavailable.status()).toBe(503);
  expect((await unavailable.json()).error.code).toMatch(
    /^(provider_unavailable|not_ready)$/,
  );
  expect(unavailable.request().postDataJSON()).toMatchObject({
    interview_id: new URL(url).pathname.split("/").pop(),
    expected_revision: expect.any(Number),
  });
  await expect(customerPage.getByRole("alert")).toBeVisible();
  expect(
    await customerPage.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(1);
  await customerPage.getByRole("button", { name: "Stop", exact: true }).click();
  await expect(customerPage.getByText("Stopped locally.")).toBeVisible();
  await customerPage.getByRole("link", { name: "Back to sound check" }).click();
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

test("same-browser invitation tabs cannot consent to the wrong interview", async ({
  browser,
  page,
}) => {
  test.skip(
    !process.env.TEST_OPERATOR_USERNAME || !process.env.TEST_OPERATOR_PASSWORD,
    "Set local operator test credentials to run the real API regression.",
  );
  await page.goto("/operator");
  await page
    .getByLabel("Username", { exact: true })
    .fill(process.env.TEST_OPERATOR_USERNAME!);
  await page
    .getByLabel("Password", { exact: true })
    .fill(process.env.TEST_OPERATOR_PASSWORD!);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  const urls: string[] = [];
  for (const suffix of ["A", "B"]) {
    await page
      .getByLabel("Customer name")
      .fill(`Synthetic tabs ${Date.now()} ${suffix}`);
    await page
      .getByLabel("Project context")
      .fill("Synthetic shared-cookie scope regression.");
    const created = page.waitForResponse(
      (response) =>
        new URL(response.url()).pathname === "/api/operator/invitations" &&
        response.request().method() === "POST",
    );
    await page
      .getByRole("button", { name: "Create private invitation" })
      .click();
    expect((await created).ok()).toBe(true);
    urls.push(await page.getByLabel("Private invitation link").inputValue());
  }
  const customer = await browser.newContext();
  try {
    const tabA = await customer.newPage();
    const tabB = await customer.newPage();
    await tabA.goto(urls[0]);
    await expect(
      tabA.getByRole("heading", { name: "Your voice, your choice." }),
    ).toBeVisible();
    await tabB.goto(urls[1]);
    await expect(
      tabB.getByRole("heading", { name: "Your voice, your choice." }),
    ).toBeVisible();
    await tabA.getByRole("checkbox", { name: /I consent/ }).check();
    const consent = tabA.waitForResponse(
      (response) =>
        new URL(response.url()).pathname === "/api/customer/consent",
    );
    await tabA
      .getByRole("button", { name: "I agree — check my sound" })
      .click();
    const rejected = await consent;
    expect(rejected.request().postDataJSON()).toEqual({
      interview_id: new URL(urls[0]).pathname.split("/").pop(),
      policy_version: "recording-v1",
    });
    expect(rejected.status()).toBe(403);
    expect((await rejected.json()).error.code).toBe("scope_changed");
    await expect(tabA.getByRole("alert")).toBeVisible();
    await expect(
      tabA.getByRole("button", { name: "Check microphone", exact: true }),
    ).toHaveCount(0);
    const current = await customer.request.get(
      new URL("/api/customer/session", urls[1]).href,
    );
    expect(current.ok()).toBe(true);
    const session = await current.json();
    expect(session.id).toBe(new URL(urls[1]).pathname.split("/").pop());
    expect(session.consented_at).toBeNull();
    expect(session.state).toBe("invited");
    await tabB.reload();
    await expect(
      tabB.getByRole("heading", { name: "Your voice, your choice." }),
    ).toBeVisible();
  } finally {
    await customer.close();
  }
});
