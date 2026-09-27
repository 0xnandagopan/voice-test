export type SessionView = {
  id: string;
  customer_label: string;
  project_context: string;
  agency_name: string;
  state:
    | "invited"
    | "consented"
    | "interviewing"
    | "recovering"
    | "processing"
    | "draft"
    | "deleted"
    | "revoked";
  revision: number;
  consented_at: string | null;
  consent_policy_version: string;
  expires_at: string;
  remaining_seconds: number;
  voice_available: boolean;
};
export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}
export async function api<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method: body === undefined ? "GET" : "POST",
    credentials: "same-origin",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  const data = await response.json().catch(() => null);
  if (!response.ok)
    throw new ApiError(
      response.status,
      data?.error?.code ?? "request_failed",
      data?.error?.message ??
        "We could not complete that request. Please try again.",
    );
  return data as T;
}
export const sessionKey = ["customer-session"];
export const sessionQuery = {
  queryKey: sessionKey,
  queryFn: () => api<SessionView>("/customer/session"),
  retry: false as const,
};
