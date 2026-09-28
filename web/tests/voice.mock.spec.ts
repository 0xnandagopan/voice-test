import { test, expect, type WebSocketRoute } from "@playwright/test";
const id = "6c28c68e-2296-4c37-b4c0-50d8bbf9e654";
test("relay readiness gates capture; controls use server revisions; Stop releases tracks", async ({
  page,
}) => {
  await page.route("**/api/customer/session", (route) =>
    route.fulfill({
      json: {
        id,
        state: "consented",
        revision: 2,
        consented_at: "2026-09-28T00:00:00Z",
        remaining_seconds: 360,
        voice_available: true,
      },
    }),
  );
  await page.route("**/api/customer/start", (route) =>
    route.fulfill({
      json: {
        ws_url: `/api/customer/interviews/${id}/live?expected_revision=2`,
      },
    }),
  );
  await page.addInitScript(() => {
    const original = navigator.mediaDevices.getUserMedia.bind(
      navigator.mediaDevices,
    );
    Object.assign(window, { micCalls: 0, tracks: [] });
    navigator.mediaDevices.getUserMedia = async (constraints) => {
      const w = window as unknown as {
        micCalls: number;
        tracks: MediaStreamTrack[];
      };
      w.micCalls++;
      const stream = await original(constraints);
      w.tracks.push(...stream.getTracks());
      return stream;
    };
  });
  let socket: WebSocketRoute | undefined;
  const commands: any[] = [];
  await page.routeWebSocket(`**/api/customer/interviews/${id}/live*`, (ws) => {
    socket = ws;
    ws.onMessage((data) => {
      const parsed = JSON.parse(String(data));
      if (parsed.type !== "audio") commands.push(parsed);
    });
  });
  await page.goto("/interview");
  await page
    .getByRole("button", { name: "Start interview", exact: true })
    .click();
  await expect.poll(() => Boolean(socket)).toBe(true);
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(0);
  socket!.send(
    JSON.stringify({
      type: "ready",
      attempt_id: "attempt",
      revision: 3,
      progress_revision: 1,
      remaining_seconds: 350,
    }),
  );
  await expect(page.getByText("Recording is active.")).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(1);
  socket!.send(
    JSON.stringify({
      type: "caption",
      speaker: "customer",
      item_id: "turn-1",
      text: "A synthetic answer.",
      final: true,
    }),
  );
  await expect(page.getByText("A synthetic answer.")).toBeVisible();
  await page.getByRole("button", { name: "Repeat question" }).click();
  await expect.poll(() => commands.length).toBe(1);
  expect(commands[0]).toMatchObject({
    type: "control",
    action: "repeat",
    expected_revision: 3,
    expected_progress_revision: 1,
  });
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  await expect(page.getByText("Stopped locally.")).toBeVisible();
  expect(
    await page.evaluate(() =>
      (window as unknown as { tracks: MediaStreamTrack[] }).tracks.every(
        (track) => track.readyState === "ended",
      ),
    ),
  ).toBe(true);
  expect(commands.some((command) => command.type === "stop")).toBe(true);
});
test("Stop while relay is connecting prevents subsequent microphone capture", async ({
  page,
}) => {
  await page.route("**/api/customer/session", (route) =>
    route.fulfill({
      json: {
        id,
        state: "consented",
        revision: 2,
        consented_at: "2026-09-28T00:00:00Z",
        remaining_seconds: 360,
        voice_available: true,
      },
    }),
  );
  await page.route("**/api/customer/start", (route) =>
    route.fulfill({
      json: {
        ws_url: `/api/customer/interviews/${id}/live?expected_revision=2`,
      },
    }),
  );
  await page.addInitScript(() => {
    Object.assign(window, { micCalls: 0 });
    navigator.mediaDevices.getUserMedia = async () => {
      (window as unknown as { micCalls: number }).micCalls++;
      throw new Error("Should not capture");
    };
  });
  let socket: WebSocketRoute | undefined;
  await page.routeWebSocket(`**/api/customer/interviews/${id}/live*`, (ws) => {
    socket = ws;
  });
  await page.goto("/interview");
  await page
    .getByRole("button", { name: "Start interview", exact: true })
    .click();
  await expect.poll(() => Boolean(socket)).toBe(true);
  await page.getByRole("button", { name: "Stop", exact: true }).click();
  socket!.send(
    JSON.stringify({
      type: "ready",
      revision: 3,
      progress_revision: 1,
      remaining_seconds: 350,
    }),
  );
  await expect(page.getByText("Stopped locally.")).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as unknown as { micCalls: number }).micCalls,
    ),
  ).toBe(0);
});
