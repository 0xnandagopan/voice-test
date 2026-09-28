import { test, expect, type Page } from "@playwright/test";
import type { Evidence, WorkflowView } from "../src/workflow";
const id = "6c28c68e-2296-4c37-b4c0-50d8bbf9e654";
const initial: WorkflowView = {
  revisions: { workflow: 3, content: 1, evidence: 1 },
  evidence_available: true,
  content: {
    text: "The new site probably saves us two hours in a busy week.",
    attribution: "Alex, Studio North",
    clips: [],
  },
  check: "supported",
  approval: null,
  published_approval_id: null,
  declined: false,
  transcript_corrections: {},
};
async function review(
  page: Page,
  options: { unverified?: boolean; stale?: boolean; missing?: boolean } = {},
) {
  let state = structuredClone(initial);
  if (options.unverified) state.evidence_available = false;
  const requests: Record<string, any>[] = [];
  let conflict = Boolean(options.stale);
  let assessment: Evidence["assessment"] = null;
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/customer/session")
      return route.fulfill({
        json: {
          id,
          state: "recovering",
          consented_at: "2026-09-28T00:00:00Z",
          remaining_seconds: 260,
        },
      });
    if (path.endsWith("/workflow")) {
      if (route.request().method() === "GET")
        return route.fulfill({ json: state });
      const body = route.request().postDataJSON();
      requests.push(body);
      expect(body.interview_id).toBe(id);
      if (conflict) {
        conflict = false;
        state.revisions.workflow++;
        state.revisions.content++;
        state.content!.text = "A newer saved version.";
        return route.fulfill({
          status: 409,
          json: {
            error: { code: "stale_revision", message: "Version changed." },
          },
        });
      }
      expect(body.expected).toEqual(state.revisions);
      state.revisions.workflow++;
      if (body.action.type === "save") {
        state.content = body.action.content;
        state.revisions.content++;
        state.check = "pending";
        state.approval = null;
      }
      if (body.action.type === "approve") {
        state.approval = {
          id: "approval-id",
          content_revision: state.revisions.content,
          evidence_revision: state.revisions.evidence,
          content: state.content!,
          approved_at: "2026-09-28T01:00:00Z",
        };
      }
      if (body.action.type === "correct_transcript") {
        state.transcript_corrections[body.action.source_id] = body.action.text;
        state.revisions.evidence++;
        state.check = "pending";
        state.approval = null;
      }
      if (body.action.type === "decline") state.declined = true;
      return route.fulfill({
        json: { request_id: body.request_id, replayed: false, state },
      });
    }
    if (path.endsWith("/evidence"))
      return route.fulfill({
        json: {
          evidence_revision: state.revisions.evidence,
          assessment:
            assessment?.content_revision === state.revisions.content &&
            assessment?.evidence_revision === state.revisions.evidence
              ? assessment
              : null,
          sources: [
            {
              source_id: "answer/1",
              attempt_id: "attempt",
              text: "It probably saves two hours in a busy week.",
              corrected_text: null,
              speaker: "customer",
              start_ms: 0,
              end_ms: 4000,
              playback_available: true,
              alignment_verified: !options.unverified,
            },
          ],
          jobs: options.missing
            ? [
                {
                  id: "job",
                  kind: "import_evidence",
                  status: "failed",
                  error_code: "unavailable",
                },
              ]
            : [],
        },
      });
    if (path.endsWith("/recovery"))
      return route.fulfill({
        json: {
          interview_id: id,
          interview_revision: 3,
          time_consumed_seconds: 100,
          attempts: options.missing
            ? [
                {
                  provider_attempt_id: "attempt",
                  status: "recording_artifacts_unavailable",
                  untranscribed_audio_ranges_ms: [],
                },
              ]
            : [],
          recorded_utterances: [],
          unresolved_answers: [],
          requires_customer_confirmation: true,
          may_advance_progress: false,
          recommended_action: "confirm_recovered_answers_before_resume",
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { code: "not_found", message: "Unavailable" } },
    });
  });
  return {
    requests,
    setAssessment(value: Evidence["assessment"]) {
      assessment = value;
    },
    setCheck(value: WorkflowView["check"]) {
      state.check = value;
      state.revisions.workflow++;
    },
    get state() {
      return state;
    },
  };
}
test("text-only exact approval requires explicit confirmation and survives reload", async ({
  page,
}) => {
  const fixture = await review(page);
  await page.goto(`/review/${id}`);
  await expect(page.getByLabel("Testimonial text")).toHaveValue(
    initial.content!.text,
  );
  await expect(
    page.getByRole("checkbox", { name: "Include audio clips" }),
  ).not.toBeChecked();
  await expect(
    page.getByRole("button", { name: "Approve exact testimonial" }),
  ).toBeDisabled();
  await page.getByRole("checkbox", { name: /I approve this exact/ }).check();
  await page.getByRole("button", { name: "Approve exact testimonial" }).click();
  await expect(
    page.getByText("You approved this saved version."),
  ).toBeVisible();
  expect(fixture.requests[0].action).toEqual({ type: "approve" });
  expect(fixture.state.approval!.content.clips).toEqual([]);
  await page.reload();
  await expect(
    page.getByText("You approved this saved version."),
  ).toBeVisible();
  expect(fixture.requests).toHaveLength(1);
});
test("editing saves exact text, waits for support and blocks unsupported claims", async ({
  page,
}) => {
  const fixture = await review(page);
  await page.goto(`/review/${id}`);
  await page
    .getByLabel("Testimonial text")
    .fill("It always saves ten hours every week.");
  await expect(
    page.getByRole("button", { name: "Approve exact testimonial" }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Support checking is pending.")).toBeVisible();
  fixture.setCheck("unsupported");
  await page.getByRole("button", { name: "Refresh saved status" }).click();
  await expect(
    page.getByText("The saved text contains claims that are not supported."),
  ).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: /I approve this exact/ }),
  ).toBeDisabled();
  expect(fixture.requests[0].action.content.text).toBe(
    "It always saves ten hours every week.",
  );
});
test("a stale save preserves local input and requires deliberate reconciliation", async ({
  page,
}) => {
  const fixture = await review(page, { stale: true });
  await page.goto(`/review/${id}`);
  await page.getByLabel("Testimonial text").fill("My unsaved edit.");
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(
    page.getByRole("heading", { name: "Latest saved text" }),
  ).toBeVisible();
  await expect(page.getByLabel("Testimonial text")).toHaveValue(
    "My unsaved edit.",
  );
  await expect(
    page.getByRole("button", { name: "Save changes" }),
  ).toBeDisabled();
  await page
    .getByRole("button", { name: "Keep my edits against latest version" })
    .click();
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText("Support checking is pending.")).toBeVisible();
  expect(fixture.requests[1].expected.content).toBe(2);
  expect(fixture.requests[1].action.content.text).toBe("My unsaved edit.");
});
test("transcript corrections stay separate and invalidate support", async ({
  page,
}) => {
  const fixture = await review(page);
  await page.goto(`/review/${id}`);
  await page.getByText("Correct this transcript", { exact: true }).click();
  await page
    .getByLabel("Correction for source 1")
    .fill("It probably saves one hour in a busy week.");
  await page
    .getByRole("button", { name: "Save transcript correction" })
    .click();
  await expect(
    page.getByText("It probably saves two hours in a busy week.", {
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    page.getByText("Saved correction", { exact: true }),
  ).toBeVisible();
  await expect(page.getByLabel("Testimonial text")).toHaveValue(
    initial.content!.text,
  );
  await expect(
    page.getByRole("checkbox", { name: /I approve this exact/ }),
  ).toBeDisabled();
  expect(fixture.requests[0].action).toEqual({
    type: "correct_transcript",
    source_id: "answer/1",
    text: "It probably saves one hour in a busy week.",
  });
  expect(await page.locator("audio").getAttribute("src")).toContain(
    "answer%2F1/audio",
  );
});
test("unverified evidence and exhausted recovery stay blocked on mobile", async ({
  page,
}) => {
  await review(page, { unverified: true, missing: true });
  await page.setViewportSize({ width: 393, height: 851 });
  await page.goto(`/review/${id}`);
  await expect(
    page.getByText("Some recording artifacts are unavailable."),
  ).toBeVisible();
  await expect(
    page.getByText("Recorded evidence is not yet verified for approval."),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Approve exact testimonial" }),
  ).toBeDisabled();
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth),
  ).toBeLessThanOrEqual(393);
});
test("a review URL cannot switch to a different customer cookie scope", async ({
  page,
}) => {
  const fixture = await review(page);
  await page.goto("/review/another-interview");
  await expect(
    page.getByRole("heading", { name: "Review is unavailable." }),
  ).toBeVisible();
  await expect(page.getByLabel("Testimonial text")).toHaveCount(0);
  expect(fixture.requests).toHaveLength(0);
});
test("recovery acknowledgement is explicit and never starts the microphone", async ({
  page,
}) => {
  await review(page, { unverified: true, missing: true });
  let confirmations = 0;
  await page.route("**/recovery/confirm", (route) => {
    expect(route.request().postDataJSON()).toMatchObject({
      expected_revision: 3,
      evidence_revision: 1,
      acknowledge_incomplete: true,
    });
    confirmations++;
    return route.fulfill({ json: { revision: 4, confirmed: true } });
  });
  await page.addInitScript(() => {
    navigator.mediaDevices.getUserMedia = async () => {
      throw new Error("Recovery must not request microphone");
    };
  });
  await page.goto(`/review/${id}`);
  await expect(
    page.getByRole("button", { name: "Confirm recovery", exact: true }),
  ).toBeDisabled();
  await page
    .getByRole("checkbox", { name: /I understand which answers/ })
    .check();
  await page
    .getByRole("button", { name: "Confirm recovery", exact: true })
    .click();
  await expect(
    page.getByText(
      "Recovery acknowledged. Missing or incomplete answers remain ineligible evidence.",
      { exact: true },
    ),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Continue to conversation" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Approve exact testimonial" }),
  ).toBeDisabled();
  expect(confirmations).toBe(1);
});

test("operator publishes only exact approval and public view drops withdrawn content", async ({
  page,
}) => {
  const state = structuredClone(initial);
  state.approval = {
    id: "approved-version",
    content_revision: 1,
    evidence_revision: 1,
    content: state.content!,
    approved_at: "2026-09-28T00:00:00Z",
  };
  let published = false;
  const actions: string[] = [];
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === `/api/public/${id}`)
      return published
        ? route.fulfill({
            json: {
              ...state.content,
              approved_at: state.approval!.approved_at,
            },
          })
        : route.fulfill({
            status: 404,
            json: { error: { code: "unavailable", message: "Unavailable" } },
          });
    if (path.endsWith("/workflow")) {
      if (route.request().method() === "POST") {
        const request = route.request().postDataJSON();
        actions.push(request.action.type);
        expect(request.expected).toEqual(state.revisions);
        if (request.action.type === "publish") {
          expect(request.action.approval_id).toBe("approved-version");
          published = true;
          state.published_approval_id = "approved-version";
        } else {
          published = false;
          state.published_approval_id = null;
        }
        state.revisions.workflow++;
        return route.fulfill({ json: { state } });
      }
      return route.fulfill({ json: state });
    }
    return route.fulfill({ status: 404, json: {} });
  });
  await page.goto(`/operator/interviews/${id}`);
  await expect(page.getByRole("button", { name: /Approve exact/ })).toHaveCount(
    0,
  );
  await page
    .getByRole("button", { name: "Publish approved testimonial" })
    .click();
  await page.getByRole("link", { name: "View public testimonial" }).click();
  await expect(
    page.getByText(initial.content!.text, { exact: true }),
  ).toBeVisible();
  expect(
    await page.locator('meta[name="robots"]').getAttribute("content"),
  ).toContain("noindex");
  await expect(page.locator("audio")).toHaveCount(0);
  await page.goto(`/operator/interviews/${id}`);
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Unpublish", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Publish approved testimonial" }),
  ).toBeVisible();
  await page.goto(`/t/${id}`);
  await expect(
    page.getByRole("heading", { name: "Nothing is published here." }),
  ).toBeVisible();
  await expect(
    page.getByText(initial.content!.text, { exact: true }),
  ).toHaveCount(0);
  expect(actions).toEqual(["publish", "unpublish"]);
});
test("a stale transcript correction retains input until explicitly reconciled", async ({
  page,
}) => {
  const fixture = await review(page, { stale: true });
  await page.goto(`/review/${id}`);
  await page.getByText("Correct this transcript", { exact: true }).click();
  await page
    .getByLabel("Correction for source 1")
    .fill("My carefully corrected answer.");
  await page
    .getByRole("button", { name: "Save transcript correction" })
    .click();
  await expect(page.getByLabel("Correction for source 1")).toHaveValue(
    "My carefully corrected answer.",
  );
  await expect(
    page.getByRole("button", { name: "Save transcript correction" }),
  ).toBeDisabled();
  await page
    .getByRole("button", { name: "Keep my correction against latest version" })
    .click();
  await page
    .getByRole("button", { name: "Save transcript correction" })
    .click();
  await expect(
    page.getByText("Saved correction", { exact: true }),
  ).toBeVisible();
  expect(fixture.requests[1].action.text).toBe(
    "My carefully corrected answer.",
  );
  expect(fixture.requests[1].expected.workflow).toBe(4);
});

test("provisional claim references link private sources and disappear after content changes even if an old assessment is returned", async ({
  page,
}) => {
  const fixture = await review(page, { unverified: true });
  const assessment: NonNullable<Evidence["assessment"]> = {
    kind: "support_check",
    model: "synthetic-model",
    prompt_version: "fixture",
    content_revision: 1,
    evidence_revision: 1,
    assessment: {
      claims: [
        {
          text: "<img src=x onerror=alert('unsafe')> A qualified claim.",
          verdict: "uncertain",
          sources: [
            {
              source_id: "answer/1",
              quote: "It probably saves two hours in a busy week.",
            },
          ],
          issues: ["Keep the qualification and check the recording."],
        },
      ],
      issues: ["Recording alignment still needs review."],
    },
  };
  fixture.setAssessment(assessment);
  await page.goto(`/review/${id}`);
  const suggestions = page.getByRole("region", {
    name: "Provisional claim suggestions",
  });
  await expect(suggestions).toBeVisible();
  await expect(
    suggestions.getByText(assessment.assessment.claims[0].text, {
      exact: true,
    }),
  ).toBeVisible();
  await expect(suggestions.locator("img")).toHaveCount(0);
  await expect(
    suggestions.getByText(assessment.assessment.claims[0].sources[0].quote, {
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    suggestions.getByText("Keep the qualification and check the recording."),
  ).toBeVisible();
  await expect(
    suggestions.getByRole("link", { name: "Review source 1 and recording" }),
  ).toHaveAttribute("href", "#recorded-source-1");
  await suggestions
    .getByRole("link", { name: "Review source 1 and recording" })
    .click();
  await expect(page.locator("#recorded-source-1 audio")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Approve exact testimonial" }),
  ).toBeDisabled();
  await page
    .getByLabel("Testimonial text")
    .fill("A revised claim awaiting another check.");
  await page.getByRole("button", { name: "Save changes" }).click();
  await expect(suggestions).toHaveCount(0);
  await page.route("**/evidence", (route) =>
    route.fulfill({
      json: { sources: [], jobs: [], evidence_revision: 1, assessment },
    }),
  );
  await page.getByRole("button", { name: "Refresh saved status" }).click();
  await expect(
    page.getByText("No recorded sources are available yet."),
  ).toBeVisible();
  await expect(suggestions).toHaveCount(0);
});
