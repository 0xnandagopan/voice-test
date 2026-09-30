import { test, expect, type Page, type WebSocketRoute } from "@playwright/test";

const id = "6c28c68e-2296-4c37-b4c0-50d8bbf9e654";
async function fixture(page: Page, initialState = "consented") {
  let state = initialState;
  let revision = 2;
  let socket: WebSocketRoute | undefined;
  const commands: Record<string, unknown>[] = [];
  let starts = 0;
  let confirms = 0;
  await page.addInitScript(() => {
    const original = navigator.mediaDevices.getUserMedia.bind(
      navigator.mediaDevices,
    );
    Object.assign(window, { micCalls: 0, tracks: [] });
    navigator.mediaDevices.getUserMedia = async (constraints) => {
      const observed = window as unknown as {
        micCalls: number;
        tracks: MediaStreamTrack[];
      };
      observed.micCalls++;
      const stream = await original(constraints);
      observed.tracks.push(...stream.getTracks());
      return stream;
    };
  });
  await page.route("**/api/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/customer/session")
      return route.fulfill({
        json: {
          id,
          state,
          revision,
          consented_at: "2026-09-28T00:00:00Z",
          remaining_seconds: 260,
          voice_available: true,
        },
      });
    if (path === "/api/customer/start") {
      starts++;
      return route.fulfill({
        json: {
          ws_url: `/api/customer/interviews/${id}/live?expected_revision=${revision}`,
        },
      });
    }
    if (path.endsWith("/workflow"))
      return route.fulfill({
        json: {
          revisions: { workflow: 1, content: 0, evidence: 1 },
          evidence_available: false,
          content: null,
          check: "pending",
          approval: null,
          published_approval_id: null,
          declined: false,
          transcript_corrections: {},
        },
      });
    if (path.endsWith("/evidence"))
      return route.fulfill({
        json: { sources: [], jobs: [], evidence_revision: 1 },
      });
    if (path.endsWith("/recovery/confirm")) {
      confirms++;
      expect(route.request().postDataJSON()).toMatchObject({
        expected_revision: revision,
        evidence_revision: 1,
        acknowledge_incomplete: true,
      });
      state = "consented";
      revision++;
      return route.fulfill({ json: { revision, confirmed: true } });
    }
    if (path.endsWith("/recovery"))
      return route.fulfill({
        json: {
          interview_id: id,
          interview_revision: revision,
          topic_index: 1,
          followup_counts: [1, 0, 0],
          time_consumed_seconds: 100,
          attempts: [
            {
              provider_attempt_id: "attempt",
              status: "recording_artifacts_unavailable",
              recommended_action: "review",
              untranscribed_audio_ranges_ms: [],
            },
          ],
          recorded_utterances: [],
          unresolved_answers: [],
          requires_customer_confirmation: state === "recovering",
          may_advance_progress: false,
          recommended_action: "review",
        },
      });
    return route.fulfill({
      status: 404,
      json: { error: { message: "Not mocked" } },
    });
  });
  await page.routeWebSocket(`**/api/customer/interviews/${id}/live*`, (ws) => {
    socket = ws;
    ws.onMessage((data) => {
      const message = JSON.parse(String(data));
      if (message.type !== "audio") commands.push(message);
    });
  });
  return {
    commands,
    send(event: unknown) {
      socket!.send(JSON.stringify(event));
    },
    close() {
      state = "recovering";
      revision++;
      socket!.close({ code: 1011, reason: "simulated loss" });
    },
    setState(value: string) {
      state = value;
      revision++;
    },
    starts: () => starts,
    confirms: () => confirms,
    async start() {
      await page.goto("/interview");
      await page
        .getByRole("button", { name: /^(Start|Resume) interview$/ })
        .click();
      await expect.poll(() => Boolean(socket)).toBe(true);
      this.send({
        type: "ready",
        attempt_id: "attempt",
        revision: 3,
        progress_revision: 1,
        remaining_seconds: 260,
        can_finish: false,
      });
      await expect(page.getByText("Recording is active.")).toBeVisible();
    },
  };
}
async function micCalls(page: Page) {
  return page.evaluate(
    () => (window as unknown as { micCalls: number }).micCalls,
  );
}
async function tracksEnded(page: Page) {
  return page.evaluate(() =>
    (window as unknown as { tracks: MediaStreamTrack[] }).tracks.every(
      (track) => track.readyState === "ended",
    ),
  );
}

test("Finish requires fresh server permission; stale rejection preserves recording and Stop", async ({
  page,
}) => {
  const f = await fixture(page);
  await f.start();
  await expect(
    page.getByRole("button", { name: "Finish interview", exact: true }),
  ).toBeDisabled();
  f.send({
    type: "state",
    revision: 4,
    progress_revision: 2,
    remaining_seconds: 220,
    can_finish: true,
  });
  await page
    .getByRole("button", { name: "Finish interview", exact: true })
    .click();
  await expect.poll(() => f.commands.length).toBe(1);
  expect(f.commands[0]).toMatchObject({
    type: "control",
    action: "finish",
    expected_revision: 4,
    expected_progress_revision: 2,
  });
  await expect(
    page.getByRole("button", { name: "Finishing…", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "Pause interview", exact: true }),
  ).toBeEnabled();
  f.send({ type: "control_rejected", code: "stale_revision" });
  f.send({
    type: "state",
    revision: 5,
    progress_revision: 3,
    remaining_seconds: 218,
    can_finish: false,
  });
  await expect(
    page.getByText("That control was not applied", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Finish interview", exact: true }),
  ).toBeDisabled();
  await expect(page.getByText("Recording is active.")).toBeVisible();
  expect(await tracksEnded(page)).toBe(false);
  await page
    .getByRole("button", { name: "Pause interview", exact: true })
    .click();
  expect(await tracksEnded(page)).toBe(true);
});

test("only acknowledged Finish reports completion and releases microphone", async ({
  page,
}) => {
  const f = await fixture(page);
  await f.start();
  f.send({
    type: "state",
    revision: 4,
    progress_revision: 2,
    remaining_seconds: 220,
    can_finish: true,
  });
  await page
    .getByRole("button", { name: "Finish interview", exact: true })
    .click();
  await expect(page.getByText("Your interview is complete.")).toHaveCount(0);
  f.setState("completed");
  f.send({ type: "ended", reason: "explicit_finish", recovery_required: true });
  await expect(
    page.getByRole("heading", { name: "Your interview is complete." }),
  ).toBeVisible();
  await expect(
    page.getByText("completion does not approve or publish", { exact: false }),
  ).toBeVisible();
  expect(await tracksEnded(page)).toBe(true);
  await expect(
    page.getByRole("button", { name: /^(Start|Resume) interview$/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Pause interview", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Review recording", exact: true }),
  ).toBeVisible();
  await expect(page.getByText(/minutes remaining/)).toHaveCount(0);
  await page.reload();
  await expect(
    page.getByRole("button", { name: /^(Start|Resume) interview$/ }),
  ).toHaveCount(0);
  expect(await micCalls(page)).toBe(0);
});

test("connection loss releases capture; reload and recovery acknowledgement never restart it", async ({
  page,
}) => {
  const f = await fixture(page);
  await f.start();
  f.close();
  await expect(
    page.getByText("The connection stopped.", { exact: false }),
  ).toBeVisible();
  expect(await tracksEnded(page)).toBe(true);
  await page.reload();
  await expect(
    page.getByRole("button", { name: /^(Start|Resume) interview$/ }),
  ).toHaveCount(0);
  expect(await micCalls(page)).toBe(0);
  await page
    .getByRole("link", { name: "Review recording recovery", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Confirm recovery" }),
  ).toBeDisabled();
  await page
    .getByRole("checkbox", {
      name: "I understand which answers may be missing",
    })
    .check();
  await page.getByRole("button", { name: "Confirm recovery" }).click();
  await expect.poll(() => f.confirms()).toBe(1);
  expect(await micCalls(page)).toBe(0);
  await page.getByRole("link", { name: "Continue to conversation" }).click();
  await expect(
    page.getByRole("button", { name: /^(Start|Resume) interview$/ }),
  ).toBeEnabled();
  expect(await micCalls(page)).toBe(0);
  expect(f.starts()).toBe(1);
});

test("budget exhaustion ends capture without claiming user completion", async ({
  page,
}) => {
  const f = await fixture(page);
  await f.start();
  f.setState("recovering");
  f.send({
    type: "ended",
    reason: "budget_exhausted",
    recovery_required: true,
  });
  await expect(
    page.getByRole("heading", { name: "Review recording recovery." }),
  ).toBeVisible();
  expect(await tracksEnded(page)).toBe(true);
  await expect(
    page.getByText("Your six-minute interview allowance is used.", {
      exact: false,
    }),
  ).toBeVisible();
  await expect(page.getByText("Your interview is complete.")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /^(Start|Resume) interview$/ }),
  ).toHaveCount(0);
});

test("idle conversation offers separated navigation and no redundant Stop", async ({
  page,
}) => {
  await fixture(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/interview");
  await expect(
    page.getByRole("button", { name: "Resume interview", exact: true }),
  ).toBeEnabled();
  await expect(
    page.getByRole("button", { name: "Pause interview", exact: true }),
  ).toHaveCount(0);
  const review = page.getByRole("link", {
    name: "View saved recording and review",
    exact: true,
  });
  const sound = page.getByRole("link", {
    name: "Back to sound check",
    exact: true,
  });
  await expect(review).toBeVisible();
  await expect(sound).toBeVisible();
  const reviewBox = (await review.boundingBox())!;
  const soundBox = (await sound.boundingBox())!;
  expect(
    soundBox.y >= reviewBox.y + reviewBox.height + 10 ||
      soundBox.x >= reviewBox.x + reviewBox.width + 10,
  ).toBe(true);
  expect(await micCalls(page)).toBe(0);
});

test("Stop offers recovery and keeps refreshing until server finalization settles", async ({
  page,
}) => {
  const f = await fixture(page);
  await f.start();
  f.setState("interviewing");
  await page
    .getByRole("button", { name: "Pause interview", exact: true })
    .click();
  await expect(
    page.getByText("Paused locally.", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Pause interview", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /^(Start|Resume) interview$/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("link", { name: "Review recording recovery", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("link", { name: "Back to sound check", exact: true }),
  ).toHaveCount(0);
  await expect(
    page.getByText("A conversation may still be active", { exact: false }),
  ).toBeVisible();
  f.setState("recovering");
  await expect(
    page.getByText("Review and acknowledge recording recovery", {
      exact: false,
    }),
  ).toBeVisible();
  await expect(
    page.getByText("A conversation may still be active", { exact: false }),
  ).toHaveCount(0);
  expect(await tracksEnded(page)).toBe(true);
});

for (const width of [320, 390, 768]) {
  test(`conversation controls remain reachable below long captions at ${width}px`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height: 740 });
    const f = await fixture(page);
    await f.start();
    for (let i = 0; i < 25; i++) {
      f.send({
        type: "caption",
        speaker: "customer",
        item_id: `turn-${i}`,
        text: `Synthetic answer ${i}. We worked together and discussed the project in detail.`,
        final: true,
      });
    }
    f.send({
      type: "state",
      revision: 4,
      progress_revision: 2,
      remaining_seconds: 120,
      can_finish: true,
    });
    await expect(
      page.getByText("Synthetic answer 24.", { exact: false }),
    ).toBeVisible();
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    for (const name of [
      "Pause interview",
      "Repeat question",
      "Skip question",
      "Finish interview",
    ]) {
      const button = page.getByRole("button", { name, exact: true });
      await expect(button).toBeInViewport();
      const box = (await button.boundingBox())!;
      expect(box.x).toBeGreaterThanOrEqual(0);
      expect(box.x + box.width).toBeLessThanOrEqual(width);
      expect(box.height).toBeGreaterThanOrEqual(44);
    }
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth),
    ).toBeLessThanOrEqual(width);
    await page
      .getByRole("button", { name: "Finish interview", exact: true })
      .click();
    await expect
      .poll(() => f.commands.some((command) => command.action === "finish"))
      .toBe(true);
    f.setState("completed");
    f.send({
      type: "ended",
      reason: "explicit_finish",
      recovery_required: true,
    });
    await expect(
      page.getByRole("button", { name: "Pause interview" }),
    ).toHaveCount(0);
    await expect(
      page.getByRole("link", { name: "Review recording", exact: true }),
    ).toBeInViewport();
    expect(await tracksEnded(page)).toBe(true);
  });
}

test("question card and message transcript preserve exact caption text and turn order", async ({
  page,
}) => {
  const f = await fixture(page);
  await f.start();
  const question = "What, if anything, changed for your studio?";
  f.send({
    type: "caption",
    speaker: "interviewer",
    item_id: "20",
    text: "What, if anything, changed",
    final: false,
  });
  await expect(page.locator(".current-question")).toHaveText(
    "What, if anything, changed",
  );
  f.send({
    type: "caption",
    speaker: "interviewer",
    item_id: "20",
    text: question,
    final: true,
  });
  const answer =
    "Um, we saw 12—not 20—new enquiries.\nIt's early; we don't know the results yet.";
  f.send({
    type: "caption",
    speaker: "customer",
    item_id: "2",
    text: answer,
    final: false,
  });
  f.send({
    type: "caption",
    speaker: "customer",
    item_id: "2",
    text: answer,
    final: true,
  });
  await expect(page.locator(".current-question")).toHaveText(question);
  const messages = page
    .getByRole("log", { name: "Live transcript" })
    .locator(".transcript-message");
  await expect(messages).toHaveCount(2);
  expect(await messages.nth(0).locator("p").textContent()).toBe(question);
  expect(await messages.nth(1).locator("p").textContent()).toBe(answer);
  await expect(page.getByText("Speaking…", { exact: true })).toHaveCount(0);
  f.send({
    type: "caption",
    speaker: "interviewer",
    item_id: "3",
    text: "What could have worked better?",
    final: true,
  });
  await expect(page.locator(".current-question")).toHaveText(
    "What could have worked better?",
  );
  await expect(messages).toHaveCount(3);
  await page
    .getByRole("button", { name: "Skip question", exact: true })
    .click();
  await expect
    .poll(() => f.commands.some((c) => c.action === "skip"))
    .toBe(true);
  await page
    .getByRole("button", { name: "Pause interview", exact: true })
    .click();
  await expect(
    page.getByRole("link", { name: "Resume interview", exact: true }),
  ).toHaveAttribute("href", `/review/${id}#recordings`);
  expect(await tracksEnded(page)).toBe(true);
});
