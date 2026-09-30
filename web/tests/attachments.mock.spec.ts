import { test, expect, type Page } from "@playwright/test";
const invitation = {
  id: "6c28c68e-2296-4c37-b4c0-50d8bbf9e654",
  customer_label: "Alex",
  project_context: "A new website",
  agency_name: "Example Agency",
  state: "invited",
  revision: 1,
  consented_at: null,
  consent_policy_version: "recording-v1",
  expires_at: "2099-10-11T00:00:00Z",
  remaining_seconds: 360,
  voice_available: true,
  interview_preparation: "queued",
};
function file(name: string, content: string | Buffer) {
  return {
    name,
    mimeType: "text/plain",
    buffer: typeof content === "string" ? Buffer.from(content) : content,
  };
}
async function dashboard(
  page: Page,
  options: { failedCreate?: boolean; preparation?: string } = {},
) {
  const posts: Record<string, unknown>[] = [];
  let preparation = options.preparation;
  let attempts = 0;
  let retries = 0;
  await page.route("**/api/operator/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/me"))
      return route.fulfill({ json: { username: "operator" } });
    if (path.endsWith("/prepare")) {
      retries++;
      preparation = "queued";
      return route.fulfill({ json: { ok: true } });
    }
    if (path.endsWith("/invitations") && route.request().method() === "POST") {
      posts.push(route.request().postDataJSON());
      attempts++;
      if (options.failedCreate && attempts === 1)
        return route.fulfill({
          status: 503,
          json: {
            error: {
              message: "Creation unavailable. Try again.",
              code: "unavailable",
            },
          },
        });
      return route.fulfill({
        json: {
          invitation,
          private_url: "https://example.test/i/test#token=synthetic",
        },
      });
    }
    return route.fulfill({
      json: {
        invitations: preparation
          ? [{ ...invitation, interview_preparation: preparation }]
          : [],
      },
    });
  });
  await page.goto("/operator");
  await page.getByLabel("Customer name").fill("Alex");
  await page
    .getByLabel("Project context", { exact: true })
    .fill("A new website");
  return {
    posts,
    retries: () => retries,
    ready: () => {
      preparation = "ready";
    },
  };
}
test("attachments preserve input on API failure and clear only after successful creation", async ({
  page,
}) => {
  const mock = await dashboard(page, { failedCreate: true });
  await page
    .getByLabel("Attach context files")
    .setInputFiles([
      file("profile.md", "# Client\nA regional art studio"),
      file("contract.json", '{"scope":"website"}'),
      file("notes.txt", "Please ask about onboarding."),
    ]);
  await expect(
    page
      .getByRole("list", { name: "Selected context files" })
      .getByRole("listitem"),
  ).toHaveCount(3);
  await page.getByRole("button", { name: "Remove notes.txt" }).click();
  await page.getByRole("button", { name: "Create private invitation" }).click();
  await expect(page.getByRole("alert")).toContainText("Creation unavailable");
  await expect(page.getByLabel("Customer name")).toHaveValue("Alex");
  await expect(
    page
      .getByRole("list", { name: "Selected context files" })
      .getByRole("listitem"),
  ).toHaveCount(2);
  await page.getByRole("button", { name: "Create private invitation" }).click();
  await expect(
    page.getByLabel("Private invitation link", { exact: true }),
  ).toBeVisible();
  expect(mock.posts).toHaveLength(2);
  expect(mock.posts[0]).toEqual(mock.posts[1]);
  expect(mock.posts[1].context_attachments).toEqual([
    { name: "profile.md", content: "# Client\nA regional art studio" },
    { name: "contract.json", content: '{"scope":"website"}' },
  ]);
  await expect(page.getByLabel("Customer name")).toHaveValue("");
  await expect(
    page.getByRole("list", { name: "Selected context files" }),
  ).toHaveCount(0);
  expect(
    await page.evaluate(() => ({
      local: { ...localStorage },
      session: { ...sessionStorage },
    })),
  ).toEqual({ local: {}, session: {} });
});
test("file validation rejects invalid batches without losing earlier input", async ({
  page,
}) => {
  await dashboard(page);
  const picker = page.getByLabel("Attach context files");
  await picker.setInputFiles(file("profile.md", "Valid existing context"));
  const rejected = [
    [file("report.pdf", "PDF"), "Choose .txt"],
    [file("PROFILE.MD", "duplicate"), "already selected"],
    [file("invalid.json", "{broken"), "JSON file is not valid"],
    [file("invalid.txt", Buffer.from([0xc3, 0x28])), "valid UTF-8"],
    [file("binary.txt", Buffer.from([65, 0, 66])), "control characters"],
    [file("too-large.txt", "é".repeat(16385)), "32 KiB"],
    [file("empty.txt", " \n"), "readable text"],
  ] as const;
  for (const [entry, error] of rejected) {
    await picker.setInputFiles(entry);
    await expect(page.getByRole("alert")).toContainText(error);
    await expect(
      page.getByRole("button", { name: "Remove profile.md" }),
    ).toBeVisible();
    await expect(
      page.getByRole("textbox", { name: "Project context", exact: true }),
    ).toHaveValue("A new website");
  }
  await picker.setInputFiles(
    Array.from({ length: 5 }, (_, n) => file(`extra${n}.txt`, "extra")),
  );
  await expect(page.getByRole("alert")).toContainText("up to five files");
  await picker.setInputFiles(
    Array.from({ length: 3 }, (_, n) =>
      file(`large${n}.txt`, "a".repeat(32768)),
    ),
  );
  await expect(page.getByRole("alert")).toContainText("96 KiB");
  await expect(
    page
      .getByRole("list", { name: "Selected context files" })
      .getByRole("listitem"),
  ).toHaveCount(1);
});
test("dropzone supports keyboard selection and file drops on a mobile viewport", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await dashboard(page);
  const dropzone = page.getByRole("button", { name: /Drop or attach files/ });
  await dropzone.focus();
  const choice = page.waitForEvent("filechooser");
  await page.keyboard.press("Enter");
  await (await choice).setFiles(file("profile.txt", "A studio"));
  await expect(
    page.getByRole("button", { name: "Remove profile.txt" }),
  ).toBeVisible();
  const transfer = await page.evaluateHandle(() => {
    const data = new DataTransfer();
    data.items.add(new File(["A launch"], "launch.md", { type: "text/plain" }));
    return data;
  });
  await dropzone.dispatchEvent("drop", { dataTransfer: transfer });
  await expect(
    page.getByRole("button", { name: "Remove launch.md" }),
  ).toBeVisible();
  const box = await dropzone.boundingBox();
  expect(box!.x).toBeGreaterThanOrEqual(0);
  expect(box!.x + box!.width).toBeLessThanOrEqual(390);
});
test("preparation failures retry and pending rows refresh to ready", async ({
  page,
}) => {
  const mock = await dashboard(page, { preparation: "failed" });
  await expect(
    page.getByText("Preparation failed", { exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Retry interview preparation" })
    .click();
  await expect(
    page.getByText("Preparing interview", { exact: true }),
  ).toBeVisible();
  expect(mock.retries()).toBe(1);
  mock.ready();
  await expect(page.getByTitle("Interview ready", { exact: true })).toBeVisible(
    {
      timeout: 8000,
    },
  );
  await expect(
    page.getByRole("button", { name: "Retry interview preparation" }),
  ).toHaveCount(0);
});

test("customer preparation refreshes without microphone capture or exposing files", async ({
  page,
}) => {
  let ready = false;
  await page.addInitScript(() => {
    Object.assign(window, { microphoneRequests: 0 });
    navigator.mediaDevices.getUserMedia = async () => {
      (window as unknown as { microphoneRequests: number })
        .microphoneRequests++;
      throw new Error("Microphone must not start automatically");
    };
  });
  await page.route("**/api/customer/session", (route) =>
    route.fulfill({
      json: {
        ...invitation,
        state: "consented",
        consented_at: "2026-09-30T00:00:00Z",
        interview_preparation: ready ? "ready" : "queued",
        voice_available: ready,
      },
    }),
  );
  await page.goto("/interview");
  await expect(
    page.getByText(/Preparing your project-specific interview/),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Start interview", exact: true }),
  ).toBeDisabled();
  ready = true;
  await expect(
    page.getByRole("button", { name: "Start interview", exact: true }),
  ).toBeEnabled({ timeout: 5000 });
  expect(
    await page.evaluate(
      () =>
        (window as unknown as { microphoneRequests: number })
          .microphoneRequests,
    ),
  ).toBe(0);
  await expect(page.getByText("Attachment for the agent")).toHaveCount(0);
});
