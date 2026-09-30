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
  await expect(
    customerPage.getByRole("button", { name: "Start interview", exact: true }),
  ).toBeDisabled();
  await expect(
    customerPage.getByText(/Preparing your project-specific interview/),
  ).toBeVisible();
  // UI availability is advisory: the API must independently reject a direct Start.
  const session = await (
    await customer.request.get(
      new URL("/api/customer/session", process.env.TEST_API_ORIGIN ?? url).href,
    )
  ).json();
  expect(session.interview_preparation).toBe("queued");
  const unavailable = await customer.request.post(
    new URL("/api/customer/start", process.env.TEST_API_ORIGIN ?? url).href,
    {
      headers: { Origin: new URL(url).origin },
      data: { interview_id: session.id, expected_revision: session.revision },
    },
  );
  expect(unavailable.status()).toBe(503);
  expect((await unavailable.json()).error.code).toMatch(
    /^(provider_unavailable|not_ready)$/,
  );
  expect(
    await customerPage.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(1);
  await expect(
    customerPage.getByRole("button", { name: "Pause interview", exact: true }),
  ).toHaveCount(0);
  await customerPage.getByRole("link", { name: "Back to sound check" }).click();
  await page.reload();
  page.once("dialog", (dialog) => dialog.accept());
  await page
    .getByRole("listitem")
    .filter({ hasText: label })
    .getByRole("button", { name: /Revoke invitation for/ })
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
      new URL("/api/customer/session", process.env.TEST_API_ORIGIN ?? urls[1])
        .href,
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

test("real API private review saves edits across reload and reconciles a second tab without approving unverified evidence", async ({
  browser,
  page,
}) => {
  test.skip(
    !process.env.TEST_OPERATOR_USERNAME || !process.env.TEST_OPERATOR_PASSWORD,
    "Set local operator test credentials for the connected review journey.",
  );
  await page.goto("/operator");
  await page
    .getByLabel("Username", { exact: true })
    .fill(process.env.TEST_OPERATOR_USERNAME!);
  await page
    .getByLabel("Password", { exact: true })
    .fill(process.env.TEST_OPERATOR_PASSWORD!);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByLabel("Customer name").fill(`Synthetic review ${Date.now()}`);
  await page
    .getByLabel("Project context")
    .fill(
      "Synthetic manual draft and conflict regression; no recording or provider request.",
    );
  await page.getByRole("button", { name: "Create private invitation" }).click();
  const invitation = await page
    .getByLabel("Private invitation link")
    .inputValue();
  const context = await browser.newContext();
  try {
    const tabA = await context.newPage();
    await tabA.goto(invitation);
    await tabA.getByRole("checkbox", { name: /I consent/ }).check();
    await tabA
      .getByRole("button", { name: "I agree — check my sound" })
      .click();
    await tabA
      .getByRole("link", { name: "View saved recording and review" })
      .click();
    await expect(
      tabA.getByText("No recorded sources are available yet."),
    ).toBeHidden();
    await tabA
      .getByText("View recording and transcript (optional)", { exact: true })
      .click();
    await expect(
      tabA.getByText("No recorded sources are available yet."),
    ).toBeVisible();
    await tabA
      .getByText("View recording and transcript (optional)", { exact: true })
      .click();
    await expect(
      tabA.getByRole("button", { name: "Prepare provisional draft" }),
    ).toBeDisabled();
    await tabA.getByRole("button", { name: "Write my own draft" }).click();
    const savedText = "A synthetic private draft awaiting recorded support.";
    await tabA.getByLabel("Testimonial text").fill(savedText);
    await tabA
      .getByLabel("Attribution", { exact: true })
      .fill("Synthetic participant");
    const save = tabA.waitForResponse(
      (response) =>
        new URL(response.url()).pathname.endsWith("/workflow") &&
        response.request().method() === "POST",
    );
    await tabA.getByRole("button", { name: "Save changes" }).click();
    expect((await save).status()).toBe(200);
    await tabA.reload();
    await expect(tabA.getByLabel("Testimonial text")).toHaveValue(savedText);
    await expect(
      tabA.getByRole("checkbox", { name: /I approve this exact/ }),
    ).toBeDisabled();
    await expect(
      tabA.getByRole("button", { name: "Approve exact testimonial" }),
    ).toBeDisabled();
    const tabB = await context.newPage();
    await tabB.goto(tabA.url());
    await expect(tabB.getByLabel("Testimonial text")).toHaveValue(savedText);
    await tabA
      .getByLabel("Testimonial text")
      .fill("My local unsaved version, preserved for reconciliation.");
    await tabB
      .getByLabel("Testimonial text")
      .fill("The other tab saved this version first.");
    const saveB = tabB.waitForResponse(
      (response) =>
        new URL(response.url()).pathname.endsWith("/workflow") &&
        response.request().method() === "POST",
    );
    await tabB.getByRole("button", { name: "Save changes" }).click();
    expect((await saveB).status()).toBe(200);
    await tabA.getByRole("button", { name: "Refresh saved status" }).click();
    await expect(
      tabA.getByRole("heading", { name: "Latest saved text" }),
    ).toBeVisible();
    await expect(tabA.getByLabel("Testimonial text")).toHaveValue(
      "My local unsaved version, preserved for reconciliation.",
    );
    await expect(
      tabA.getByRole("button", { name: "Save changes" }),
    ).toBeDisabled();
    await tabA
      .getByRole("button", { name: "Keep my edits against latest version" })
      .click();
    const reconciled = tabA.waitForResponse(
      (response) =>
        new URL(response.url()).pathname.endsWith("/workflow") &&
        response.request().method() === "POST",
    );
    await tabA.getByRole("button", { name: "Save changes" }).click();
    expect((await reconciled).status()).toBe(200);
    await tabA.reload();
    await expect(tabA.getByLabel("Testimonial text")).toHaveValue(
      "My local unsaved version, preserved for reconciliation.",
    );
    await expect(
      tabA.getByRole("button", { name: "Approve exact testimonial" }),
    ).toBeDisabled();
  } finally {
    await context.close();
  }
});
