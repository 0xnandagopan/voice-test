import { test, expect, type Page } from "@playwright/test";
const id = "6c28c68e-2296-4c37-b4c0-50d8bbf9e654";
const fixture = {
  id,
  customer_label: "Alex",
  project_context: "A new website for Studio North",
  agency_name: "Example Agency",
  state: "invited",
  revision: 1,
  consented_at: null as string | null,
  consent_policy_version: "recording-v1",
  expires_at: "2026-10-11T00:00:00Z",
  remaining_seconds: 360,
  voice_available: false,
};
async function customer(page: Page) {
  let session = { ...fixture };
  const calls: string[] = [];
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    calls.push(path);
    if (path === "/api/customer/exchange")
      return route.fulfill({ json: { ok: true } });
    if (path === "/api/customer/session")
      return route.fulfill({ json: session });
    if (path === "/api/customer/consent") {
      expect(route.request().postDataJSON()).toEqual({
        interview_id: id,
        policy_version: "recording-v1",
      });
      session = {
        ...session,
        state: "consented",
        consented_at: new Date().toISOString(),
        revision: 2,
      };
      return route.fulfill({ json: session });
    }
    if (path === "/api/customer/start") {
      expect(route.request().postDataJSON()).toEqual({
        interview_id: id,
        expected_revision: 2,
      });
      return route.fulfill({
        status: 503,
        json: {
          error: {
            code: "provider_unavailable",
            message: "Live voice is not configured. No recording has started.",
          },
        },
      });
    }
    return route.fulfill({
      status: 404,
      json: { error: { code: "not_ready", message: "Not implemented" } },
    });
  });
  return calls;
}
test("customer consent gates microphone; fragment removed; denial retries with synthetic microphone", async ({
  page,
}) => {
  const calls = await customer(page);
  await page.addInitScript(() => {
    const original = navigator.mediaDevices.getUserMedia.bind(
      navigator.mediaDevices,
    );
    Object.assign(window, { micCalls: 0 });
    navigator.mediaDevices.getUserMedia = async (constraints) => {
      const win = window as unknown as { micCalls: number };
      win.micCalls++;
      if (win.micCalls === 1)
        throw new DOMException("Permission denied", "NotAllowedError");
      return original(constraints);
    };
  });
  await page.goto(`/i/${id}#token=synthetic-invitation-secret`);
  await expect(
    page.getByRole("heading", { name: "Your voice, your choice." }),
  ).toBeVisible();
  await expect(page).toHaveURL(`/i/${id}`);
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(0);
  expect(calls.filter((path) => path.endsWith("/exchange"))).toHaveLength(1);
  await expect(
    page.getByRole("button", { name: "I agree — check my sound" }),
  ).toBeDisabled();
  await page.getByRole("checkbox", { name: /I consent/ }).check();
  await page.getByRole("button", { name: "I agree — check my sound" }).click();
  await expect(
    page.getByRole("heading", { name: "Let’s check your sound." }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(0);
  await page
    .getByRole("button", { name: "Check microphone", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Retry microphone" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Retry microphone" }).click();
  await expect(page.getByText("Microphone is accessible.")).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(2);
  await expect(
    page.getByRole("button", { name: "Continue to conversation" }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "Play test tone" }).click();
  await page.getByRole("checkbox", { name: "I heard the test tone." }).check();
  await page.getByRole("button", { name: "Continue to conversation" }).click();
  await expect(
    page.getByRole("button", { name: "Start interview", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByText("Live interviews are not ready in this build.", {
      exact: false,
    }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(2);
  await expect(
    page.getByRole("button", { name: "Pause interview", exact: true }),
  ).toHaveCount(0);
});
test("failed consent does not unlock readiness or ask for microphone", async ({
  page,
}) => {
  await customer(page);
  await page.route("**/api/customer/consent", (route) =>
    route.fulfill({
      status: 503,
      json: {
        error: {
          code: "not_ready",
          message: "Consent was not saved. Try again.",
        },
      },
    }),
  );
  await page.goto(`/i/${id}#token=test`);
  await page.getByRole("checkbox", { name: /I consent/ }).check();
  await page.getByRole("button", { name: "I agree — check my sound" }).click();
  await expect(page.getByRole("alert")).toHaveText(
    "Consent was not saved. Try again.",
  );
  await expect(
    page.getByRole("button", { name: "Check microphone", exact: true }),
  ).toHaveCount(0);
});
test("revoked exchange removes secret and reports access failure", async ({
  page,
}) => {
  await page.route("**/api/customer/exchange", (route) =>
    route.fulfill({
      status: 401,
      json: {
        error: { code: "unauthorized", message: "Invitation is unavailable." },
      },
    }),
  );
  await page.goto(`/i/${id}#token=revoked-secret`);
  await expect(
    page.getByRole("heading", { name: "We couldn’t open this invitation." }),
  ).toBeVisible();
  expect(new URL(page.url()).hash).toBe("");
  await expect(page.getByRole("checkbox")).toHaveCount(0);
});
test("operator signs in, creates, copies and revokes an invitation", async ({
  page,
  context,
}) => {
  let authenticated = false;
  let invitations: (typeof fixture)[] = [];
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.route("**/api/operator/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/login")) {
      authenticated = true;
      return route.fulfill({ json: { ok: true } });
    }
    if (path.endsWith("/me"))
      return route.fulfill(
        authenticated
          ? { json: { username: "operator" } }
          : {
              status: 401,
              json: {
                error: { code: "unauthorized", message: "Sign in required" },
              },
            },
      );
    if (path.endsWith("/revoke")) {
      expect(route.request().postDataJSON()).toEqual({ expected_revision: 1 });
      invitations = [{ ...fixture, state: "revoked" }];
      return route.fulfill({ json: { ok: true } });
    }
    if (path.endsWith("/invitations") && route.request().method() === "POST") {
      expect(route.request().postDataJSON().customer_label).toBe("Alex");
      invitations = [fixture];
      return route.fulfill({
        json: {
          invitation: fixture,
          private_url: `http://localhost:5173/i/${id}#token=synthetic-secret`,
        },
      });
    }
    return route.fulfill({ json: { invitations } });
  });
  await page.goto("/operator");
  await page.getByLabel("Username", { exact: true }).fill("operator");
  await page.getByLabel("Password", { exact: true }).fill("synthetic-password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await page.getByLabel("Customer name").fill("Alex");
  await page.getByLabel("Project context").fill(fixture.project_context);
  await page.getByRole("button", { name: "Create private invitation" }).click();
  await expect(page.getByText("Alex", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Copy link", exact: true }).click();
  await expect(page.getByRole("button", { name: "Link copied" })).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toContain(
    `#token=synthetic-secret`,
  );
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Revoke", exact: true }).click();
  await expect(page.getByText("revoked", { exact: true })).toBeVisible();
});
test("mobile welcome fits viewport and never fabricates published content", async ({
  page,
}) => {
  await page.setViewportSize({ width: 393, height: 851 });
  await customer(page);
  await page.goto(`/i/${id}#token=test`);
  await expect(
    page.getByRole("heading", { name: "Your voice, your choice." }),
  ).toBeVisible();
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(393);
  await page.goto("/t/example");
  await expect(
    page.getByRole("heading", { name: "Nothing is published here." }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: /Approve|Publish/ }),
  ).toHaveCount(0);
});

test("operator retrieves each older invitation after reload without persisting secrets", async ({
  page,
  context,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const second = {
    ...fixture,
    id: "cba88a77-3355-4242-8181-882233445566",
    customer_label: "Second customer",
  };
  const expired = {
    ...fixture,
    id: "expired",
    customer_label: "Expired customer",
    expires_at: "2020-01-01T00:00:00Z",
  };
  const revoked = {
    ...fixture,
    id: "revoked",
    customer_label: "Revoked customer",
    state: "revoked",
  };
  const linkReads: string[] = [];
  await page.route("**/api/operator/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/me"))
      return route.fulfill({ json: { username: "operator" } });
    if (path.endsWith("/link")) {
      linkReads.push(path);
      return route.fulfill({
        json: {
          private_url: `https://example.test/i/${path.split("/").at(-2)}#token=synthetic-token`,
        },
      });
    }
    return route.fulfill({
      json: { invitations: [second, fixture, expired, revoked] },
    });
  });
  await page.setViewportSize({ width: 320, height: 740 });
  await page.goto("/operator");
  await expect(
    page.getByRole("button", {
      name: "Copy invitation link for Alex",
      exact: true,
    }),
  ).toBeVisible();
  expect(linkReads).toHaveLength(0);
  await page.reload();
  for (const item of [fixture, second]) {
    await page
      .getByRole("button", {
        name: `Copy invitation link for ${item.customer_label}`,
        exact: true,
      })
      .click();
    await expect
      .poll(() => page.evaluate(() => navigator.clipboard.readText()))
      .toContain(`/i/${item.id}#token=synthetic-token`);
  }
  expect(linkReads).toHaveLength(2);
  await expect(
    page.getByRole("button", {
      name: /Copy invitation link for (Expired|Revoked)/,
    }),
  ).toHaveCount(0);
  expect(
    await page.evaluate(() =>
      JSON.stringify({ ...localStorage, ...sessionStorage }),
    ),
  ).not.toContain("synthetic-token");
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(320);
});

test("invitation copy offers manual fallback and reports retrieval denial without stale URL", async ({
  page,
}) => {
  let unavailable = false;
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "clipboard", {
      value: {
        writeText: async () => {
          throw new Error("clipboard denied");
        },
      },
    });
  });
  await page.route("**/api/operator/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/me"))
      return route.fulfill({ json: { username: "operator" } });
    if (path.endsWith("/link"))
      return route.fulfill(
        unavailable
          ? {
              status: 410,
              json: {
                error: {
                  code: "unavailable",
                  message: "Invitation is unavailable.",
                },
              },
            }
          : {
              json: {
                private_url:
                  "https://example.test/i/synthetic#token=private-synthetic",
              },
            },
      );
    return route.fulfill({ json: { invitations: [fixture] } });
  });
  await page.goto("/operator");
  const copy = page.getByRole("button", {
    name: "Copy invitation link for Alex",
    exact: true,
  });
  await copy.click();
  await expect(
    page.getByLabel("Private invitation link for Alex", { exact: true }),
  ).toHaveValue("https://example.test/i/synthetic#token=private-synthetic");
  await expect(
    page.getByText("Clipboard is unavailable.", { exact: false }),
  ).toBeVisible();
  unavailable = true;
  await copy.click();
  await expect(page.getByRole("alert")).toHaveText(
    "Invitation is unavailable.",
  );
  await expect(
    page.getByLabel("Private invitation link for Alex", { exact: true }),
  ).toHaveCount(0);
});
