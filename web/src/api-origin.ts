/// <reference types="vite/client" />

/** Configuration is an origin, never an endpoint, credential or URL prefix. */
export function resolveApiOrigin(
  configured: string | undefined,
  browserOrigin: string,
): string {
  const value = configured?.trim() || browserOrigin;
  const url = new URL(value);
  const loopback = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (
    (url.protocol !== "https:" && !(url.protocol === "http:" && loopback)) ||
    url.username ||
    url.password ||
    url.pathname !== "/" ||
    url.search ||
    url.hash ||
    !/^https?:\/\/[^/\\?#]+\/?$/.test(value)
  )
    throw new Error(
      "VITE_API_ORIGIN must be an HTTPS origin (HTTP loopback is allowed for development).",
    );
  if (new URL(browserOrigin).protocol === "https:" && url.protocol !== "https:")
    throw new Error("An HTTPS frontend requires an HTTPS API origin.");
  return url.origin;
}

export function apiOrigin(): string {
  return resolveApiOrigin(
    import.meta.env.VITE_API_ORIGIN,
    window.location.origin,
  );
}

/** Accept only application API paths. Never send credentials to a supplied host. */
export function apiUrl(path: string): string {
  if (!path.startsWith("/") || path.startsWith("//") || /[\\\s#]/.test(path))
    throw new Error("The API path is invalid.");
  const origin = apiOrigin();
  const url = new URL(`/api${path}`, origin);
  if (url.origin !== origin || !url.pathname.startsWith("/api/"))
    throw new Error("The API path is invalid.");
  return origin === window.location.origin
    ? `${url.pathname}${url.search}`
    : url.href;
}

/** Microphone audio may only reach the configured application relay. */
export function relayUrl(path: string): URL {
  const origin = apiOrigin();
  const url = new URL(path, origin);
  const expected = new URL(origin);
  if (url.protocol === "http:") url.protocol = "ws:";
  if (url.protocol === "https:") url.protocol = "wss:";
  const protocol = expected.protocol === "https:" ? "wss:" : "ws:";
  if (
    url.host !== expected.host ||
    url.protocol !== protocol ||
    url.username ||
    url.password ||
    url.hash ||
    /[\\\s]/.test(path) ||
    !/^\/api\/customer\/interviews\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\/live$/i.test(
      url.pathname,
    ) ||
    [...url.searchParams.keys()].some((key) => key !== "expected_revision") ||
    url.searchParams.getAll("expected_revision").length !== 1 ||
    !/^\d+$/.test(url.searchParams.get("expected_revision") ?? "")
  )
    throw new Error("The voice relay address is invalid.");
  return url;
}
