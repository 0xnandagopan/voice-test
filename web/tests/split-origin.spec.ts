import { test, expect } from "@playwright/test";
import { createServer, type Server } from "node:http";
const appOrigin = `http://127.0.0.1:${process.env.V0_SPLIT_WEB_TEST_PORT ?? "5798"}`;
const apiOrigin = `http://127.0.0.1:${process.env.V0_SPLIT_API_TEST_PORT ?? "5799"}`;
const id = "6c28c68e-2296-4c37-b4c0-50d8bbf9e654";
const content = {
  text: "Saved testimony",
  attribution: "Test customer",
  clips: [{ id: "clip", sha256: "hash" }],
};
const calls: {
  path: string;
  cookie: string;
  method: string;
  origin: string;
}[] = [];
let server: Server;

test.beforeAll(async () => {
  server = createServer((req, res) => {
    const path = req.url ?? "";
    const cookie = req.headers.cookie ?? "";
    calls.push({
      path,
      cookie,
      method: req.method ?? "",
      origin: req.headers.origin ?? "",
    });
    res.setHeader("Access-Control-Allow-Origin", appOrigin);
    res.setHeader("Access-Control-Allow-Credentials", "true");
    res.setHeader("Access-Control-Allow-Methods", "GET,POST,OPTIONS");
    res.setHeader("Access-Control-Allow-Headers", "Content-Type");
    if (req.method === "OPTIONS") {
      res.writeHead(204);
      res.end();
      return;
    }
    const json = (body: unknown, status = 200) => {
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(JSON.stringify(body));
    };
    if (path === "/api/operator/login") {
      res.setHeader(
        "Set-Cookie",
        "operator=synthetic; HttpOnly; SameSite=Strict; Path=/api",
      );
      json({ ok: true });
    } else if (path === "/api/customer/exchange") {
      res.setHeader(
        "Set-Cookie",
        "customer=synthetic; HttpOnly; SameSite=Strict; Path=/api",
      );
      json({ ok: true });
    } else if (path.startsWith("/api/customer/")) {
      json({
        id,
        customer_label: "Alex",
        project_context: "Test project",
        agency_name: "Test agency",
        state: "invited",
        revision: 1,
        consented_at: null,
        consent_policy_version: "recording-v1",
        expires_at: "2027-01-01T00:00:00Z",
        remaining_seconds: 360,
        voice_available: false,
      });
    } else if (
      path.startsWith("/api/operator/") &&
      !cookie.includes("operator=synthetic")
    ) {
      json({ error: { code: "unauthorized", message: "Sign in" } }, 401);
    } else if (path === "/api/operator/me") {
      json({ username: "operator" });
    } else if (path === "/api/operator/invitations") {
      json({ invitations: [] });
    } else if (path.endsWith("/workflow")) {
      json({
        content,
        revisions: { workflow: 1, content: 1, evidence: 1 },
        check: "supported",
        evidence_available: true,
        approval: {
          id: "approval",
          content_revision: 1,
          evidence_revision: 1,
          content,
        },
        published_approval_id: "approval",
        declined: false,
        transcript_corrections: {},
      });
    } else if (path.endsWith("/audio")) {
      const wav = Buffer.alloc(44 + 8000);
      wav.write("RIFF");
      wav.writeUInt32LE(wav.length - 8, 4);
      wav.write("WAVEfmt ", 8);
      wav.writeUInt32LE(16, 16);
      wav.writeUInt16LE(1, 20);
      wav.writeUInt16LE(1, 22);
      wav.writeUInt32LE(8000, 24);
      wav.writeUInt32LE(8000, 28);
      wav.writeUInt16LE(1, 32);
      wav.writeUInt16LE(8, 34);
      wav.write("data", 36);
      wav.writeUInt32LE(8000, 40);
      wav.fill(128, 44);
      res.writeHead(200, {
        "Content-Type": "audio/wav",
        "Content-Length": wav.length,
      });
      res.end(wav);
    } else if (path.endsWith("/export")) {
      res.writeHead(200, {
        "Content-Type": "text/plain",
        "Content-Disposition": 'attachment; filename="testimonial.txt"',
      });
      res.end(content.text);
    } else if (path.startsWith("/api/public/")) {
      json(content);
    } else json({ error: { code: "missing", message: "Not found" } }, 404);
  });
  await new Promise<void>((resolve) =>
    server.listen(Number(new URL(apiOrigin).port), "127.0.0.1", resolve),
  );
});
test.afterAll(async () => {
  await new Promise<void>((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
});

test("split origin login, private audio and export send the API cookie; public media uses API", async ({
  page,
}) => {
  await page.goto("/operator");
  await page.getByLabel("Username", { exact: true }).fill("operator");
  await page
    .getByLabel("Password", { exact: true })
    .fill("synthetic-only-password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Invite a customer" }),
  ).toBeVisible();
  expect(
    calls.some(
      (call) =>
        call.path === "/api/operator/login" && call.method === "OPTIONS",
    ),
  ).toBe(true);
  expect(
    calls.some(
      (call) =>
        call.path === "/api/operator/invitations" &&
        call.cookie.includes("operator=synthetic") &&
        call.origin === appOrigin,
    ),
  ).toBe(true);
  await page.goto(`/operator/interviews/${id}`);
  const audio = page.getByLabel("Selected audio clip 1");
  await expect(audio).toHaveAttribute(
    "src",
    `${apiOrigin}/api/operator/interviews/${id}/clips/clip/audio`,
  );
  await audio.evaluate((element: HTMLAudioElement) => element.play());
  expect(
    calls.some(
      (call) =>
        call.path.endsWith("/clips/clip/audio") &&
        call.cookie.includes("operator=synthetic"),
    ),
  ).toBe(true);
  const download = page.waitForEvent("download");
  await page.getByRole("link", { name: "Download exact text" }).click();
  expect((await download).suggestedFilename()).toBe("testimonial.txt");
  expect(
    calls.some(
      (call) =>
        call.path.endsWith("/export") &&
        call.cookie.includes("operator=synthetic"),
    ),
  ).toBe(true);
  await page.goto(`/t/${id}`);
  await expect(page.getByLabel("Selected audio clip 1")).toHaveAttribute(
    "src",
    `${apiOrigin}/api/public/${id}/clips/clip/audio`,
  );
});

test("invitation exchange stores API customer cookie and removes invitation fragment", async ({
  page,
}) => {
  await page.goto(`/i/${id}#token=synthetic-secret`);
  await expect(
    page.getByRole("heading", { name: "Your voice, your choice." }),
  ).toBeVisible();
  await expect(page).toHaveURL(`${appOrigin}/i/${id}`);
  expect(
    calls.some(
      (call) =>
        call.path === "/api/customer/session" &&
        call.cookie.includes("customer=synthetic") &&
        call.origin === appOrigin,
    ),
  ).toBe(true);
});

test("relay opens only the configured API socket and rejects unsafe origins and routes", async ({
  page,
}) => {
  const wsAddress = `${apiOrigin.replace("http:", "ws:")}/api/customer/interviews/${id}/live?expected_revision=2`;
  await page.routeWebSocket(wsAddress, (socket) =>
    socket.send(
      JSON.stringify({
        type: "ready",
        attempt_id: "test",
        revision: 3,
        progress_revision: 1,
        remaining_seconds: 350,
      }),
    ),
  );
  await page.goto("/");
  const result = await page.evaluate(
    async ({ id, apiOrigin }) => {
      // Vite serves these modules in this dedicated development-only test server.
      // @ts-expect-error browser import resolved by Vite
      const origins = await import("/src/api-origin.ts");
      const { apiUrl, relayUrl, resolveApiOrigin } = origins;
      // @ts-expect-error browser import resolved by Vite
      const { connectRelay } = await import("/src/voice-transport.ts");
      const rejects = (action: () => unknown) => {
        try {
          action();
          return false;
        } catch {
          return true;
        }
      };
      const badOrigins = [
        "https://user:password@example.com",
        "https://example.com/path",
        "https://example.com?query",
        "https://example.com#hash",
        "http://example.com",
        "//example.com",
        "javascript:alert(1)",
      ];
      const badPaths = [
        "//attacker.test/audio",
        "/../secret",
        "/%2e%2e/secret",
        "/\\attacker.test",
        "/customer\n/session",
      ];
      const relay = `/api/customer/interviews/${id}/live?expected_revision=2`;
      const badRelays = [
        `wss://attacker.test${relay}`,
        `/api/customer/interviews/${id}/other/live?expected_revision=2`,
        `${relay}#hash`,
        `${relay}&secret=x`,
        `${relay}&expected_revision=3`,
        `/api/customer/interviews/${id}/live?expected_revision=invalid`,
        `ws://user:password@127.0.0.1:${new URL(apiOrigin).port}${relay}`,
        `${relay.replace(id, "%2e%2e")}`,
      ];
      const transport = await connectRelay(
        relay,
        () => {},
        () => {},
        new AbortController().signal,
      );
      transport.close();
      return {
        origins: badOrigins.every((value) =>
          rejects(() => resolveApiOrigin(value, window.location.origin)),
        ),
        downgrade: rejects(() =>
          resolveApiOrigin("http://localhost:3000", "https://app.example.com"),
        ),
        paths: badPaths.every((value) => rejects(() => apiUrl(value))),
        relays: badRelays.every((value) => rejects(() => relayUrl(value))),
        valid: relayUrl(relay).href,
        deployed: resolveApiOrigin(
          "https://voice-test-api.trypreview.online",
          "https://voice-test-app.trypreview.online",
        ),
      };
    },
    { id, apiOrigin },
  );
  expect(result).toEqual({
    origins: true,
    downgrade: true,
    paths: true,
    relays: true,
    valid: wsAddress,
    deployed: "https://voice-test-api.trypreview.online",
  });
});
