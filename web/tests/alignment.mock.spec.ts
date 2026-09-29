import { test, expect, type Page } from "@playwright/test";
import type { WorkflowView } from "../src/workflow";

const id = "6c28c68e-2296-4c37-b4c0-50d8bbf9e654";
const initial: WorkflowView = {
  revisions: { workflow: 3, content: 1, evidence: 1 },
  evidence_available: false,
  content: {
    text: "It probably helped.",
    attribution: "Synthetic customer",
    clips: [],
  },
  check: "pending",
  approval: null,
  published_approval_id: null,
  declined: false,
  transcript_corrections: {},
};
async function fixture(page: Page, initiallyVerified = false) {
  const state = structuredClone(initial);
  const confirmations: Record<string, unknown>[] = [];
  let verified = initiallyVerified;
  await page.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.endsWith("/workflow"))
      return route.fulfill({ json: state });
    if (url.pathname.endsWith("/evidence"))
      return route.fulfill({
        json: {
          evidence_revision: state.revisions.evidence,
          content_revision: state.revisions.content,
          sources: [
            {
              source_id: "answer/one",
              text: "It probably helped.",
              speaker: "customer",
              start_ms: 1000,
              end_ms: 3000,
              candidate_range_ms: [1000, 3000],
              verified_range_ms: verified ? [1000, 3000] : null,
              alignment_verified: verified,
            },
          ],
        },
      });
    if (url.pathname.endsWith("/alignment")) {
      const body = route.request().postDataJSON();
      expect(body.expected).toEqual(state.revisions);
      confirmations.push(body.confirmation);
      verified = true;
      state.revisions.workflow++;
      state.revisions.evidence++;
      state.evidence_available = true;
      return route.fulfill({ json: state });
    }
    if (url.pathname.endsWith("/audio")) return route.fulfill({ status: 204 });
    if (url.pathname.includes("/alignment/"))
      return route.fulfill({
        json: {
          recording_sha256: "recording-hash",
          timeline_sha256: "timeline-hash",
          source_id: "answer/one",
          source_text_sha256: "text-hash",
          source_range_ms: [
            Number(url.searchParams.get("start_ms")),
            Number(url.searchParams.get("end_ms")),
          ],
          clip_sha256: "clip-hash",
          listened: false,
          transcript_matches: false,
          complete_answer: false,
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Not available" } },
    });
  });
  return { state, confirmations };
}
async function playback(page: Page, start = 0) {
  // Synthetic browser event coverage exercises UI gates; it is not human listening evidence.
  await page
    .locator('audio[aria-label="Exact recorded answer preview"]')
    .evaluate((node, from) => {
      Object.defineProperty(node, "duration", { configurable: true, value: 2 });
      Object.defineProperty(node, "played", {
        configurable: true,
        value: { length: 1, start: () => from, end: () => 2 },
      });
      node.dispatchEvent(new Event("ended", { bubbles: true }));
    }, start);
}

test("verification requires complete playback and two explicit confirmations", async ({
  page,
}) => {
  const f = await fixture(page);
  await page.goto(`/operator/interviews/${id}`);
  await page.getByRole("button", { name: "Prepare audio preview" }).click();
  const matches = page.getByRole("checkbox", {
    name: "The original transcript matches",
  });
  const complete = page.getByRole("checkbox", {
    name: "This is the complete answer",
  });
  const verify = page.getByRole("button", { name: "Verify recorded answer" });
  await expect(matches).toBeDisabled();
  await expect(verify).toBeDisabled();
  await expect(
    page.locator('audio[aria-label="Exact recorded answer preview"]'),
  ).toHaveAttribute(
    "src",
    `/api/operator/interviews/${id}/alignment/answer%2Fone/audio?start_ms=1000&end_ms=3000&evidence_revision=1`,
  );
  await playback(page, 1.5);
  await expect(matches).toBeDisabled();
  await playback(page);
  await matches.check();
  await expect(verify).toBeDisabled();
  await complete.check();
  expect(f.confirmations).toHaveLength(0);
  await verify.click();
  await expect(
    page.getByText("Recorded answer verified.", { exact: true }),
  ).toBeVisible();
  expect(f.confirmations).toEqual([
    {
      recording_sha256: "recording-hash",
      timeline_sha256: "timeline-hash",
      source_id: "answer/one",
      source_text_sha256: "text-hash",
      source_range_ms: [1000, 3000],
      clip_sha256: "clip-hash",
      listened: true,
      transcript_matches: true,
      complete_answer: true,
    },
  ]);
  await expect(
    page.getByRole("button", { name: "Publish approved testimonial" }),
  ).toBeDisabled();
});

test("adjusting a range or receiving a new revision clears playback and confirmations", async ({
  page,
}) => {
  const f = await fixture(page);
  await page.goto(`/operator/interviews/${id}`);
  await page.getByRole("button", { name: "Prepare audio preview" }).click();
  await playback(page);
  await page
    .getByRole("checkbox", { name: "The original transcript matches" })
    .check();
  await page.getByLabel("End time (seconds)").fill("4");
  await expect(
    page.locator('audio[aria-label="Exact recorded answer preview"]'),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "Prepare audio preview" }).click();
  await expect(
    page.getByRole("checkbox", { name: "The original transcript matches" }),
  ).toBeDisabled();
  await expect(
    page.getByRole("checkbox", { name: "The original transcript matches" }),
  ).not.toBeChecked();
  await expect(
    page.locator('audio[aria-label="Exact recorded answer preview"]'),
  ).toHaveAttribute("src", /end_ms=4000/);
  f.state.revisions.workflow++;
  f.state.revisions.evidence++;
  await expect(
    page.locator('audio[aria-label="Exact recorded answer preview"]'),
  ).toHaveCount(0, { timeout: 10000 });
  expect(f.confirmations).toHaveLength(0);
});

test("public approved audio uses only snapshot-scoped bounded clip routes", async ({
  page,
}) => {
  await page.route("**/api/public/**", (route) =>
    route.fulfill({
      json: {
        text: "Approved words.",
        attribution: "Synthetic customer",
        clips: [{ id: "clip-id", sha256: "clip-sha" }],
        approved_at: "2026-09-29T00:00:00Z",
      },
    }),
  );
  await page.goto(`/t/${id}`);
  await expect(
    page.getByRole("heading", { name: "In their own words." }),
  ).toBeVisible();
  await expect(page.locator("audio")).toHaveAttribute(
    "src",
    `/api/public/${id}/clips/clip-id/audio`,
  );
  await expect(
    page.getByRole("button", { name: "Verify recorded answer" }),
  ).toHaveCount(0);
});

test("operator can publish a current approved snapshot with clips", async ({
  page,
}) => {
  const state = structuredClone(initial);
  state.evidence_available = true;
  state.check = "supported";
  state.content!.clips = [{ id: "clip-id", sha256: "clip-sha" }];
  state.approval = {
    id: "approval",
    content_revision: 1,
    evidence_revision: 1,
    content: state.content!,
    approved_at: "2026-09-29T00:00:00Z",
  };
  await page.route("**/api/**", (route) =>
    route.fulfill({
      json: route.request().url().endsWith("/evidence")
        ? { sources: [], evidence_revision: 1, content_revision: 1 }
        : state,
    }),
  );
  await page.goto(`/operator/interviews/${id}`);
  await expect(
    page.getByRole("button", { name: "Publish approved testimonial" }),
  ).toBeEnabled();
  await expect(page.locator("audio")).toHaveAttribute(
    "src",
    `/api/operator/interviews/${id}/clips/clip-id/audio`,
  );
});

test("a verified answer can be rechecked with a revised complete range", async ({
  page,
}) => {
  const f = await fixture(page, true);
  await page.goto(`/operator/interviews/${id}`);
  await expect(
    page.getByText("Recorded answer verified.", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText(
      "The listed answers are verified, but recording completeness or coverage is still unresolved.",
      { exact: false },
    ),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Review or change verified range" })
    .click();
  await expect(page.getByLabel("Start time (seconds)")).toHaveValue("1");
  await expect(page.getByLabel("End time (seconds)")).toHaveValue("3");
  await page.getByLabel("End time (seconds)").fill("4");
  await page.getByRole("button", { name: "Keep verified range" }).click();
  expect(f.confirmations).toHaveLength(0);
  await page
    .getByRole("button", { name: "Review or change verified range" })
    .click();
  await expect(page.getByLabel("End time (seconds)")).toHaveValue("3");
  await page.getByLabel("End time (seconds)").fill("4");
  await page.getByRole("button", { name: "Prepare audio preview" }).click();
  await expect(
    page.getByRole("button", { name: "Verify recorded answer" }),
  ).toBeDisabled();
  await playback(page);
  await page
    .getByRole("checkbox", { name: "The original transcript matches" })
    .check();
  await page
    .getByRole("checkbox", { name: "This is the complete answer" })
    .check();
  await page.getByRole("button", { name: "Verify recorded answer" }).click();
  await expect(
    page.getByText("Recorded answer verified.", { exact: true }),
  ).toBeVisible();
  expect(f.confirmations).toHaveLength(1);
  expect(f.confirmations[0].source_range_ms).toEqual([1000, 4000]);
});
