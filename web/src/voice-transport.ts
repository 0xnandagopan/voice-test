import { relayUrl } from "./api-origin";
import type { VoiceEvent, VoiceTransport } from "./audio";

export type RelayState = {
  revision: number;
  progress_revision: number;
  remaining_seconds: number;
  can_finish: boolean;
};
export type RelayEvent =
  | ({ type: "ready"; attempt_id: string } & RelayState)
  | ({ type: "state" } & RelayState)
  | {
      type: "caption";
      speaker: "customer" | "interviewer";
      item_id: string;
      text: string;
      final: boolean;
    }
  | { type: "ended"; reason: string; recovery_required: boolean }
  | { type: "error"; code: string }
  | { type: "control_rejected"; code: string };
export type RelayTransport = VoiceTransport & {
  control(action: "skip" | "repeat" | "finish"): void;
};
/** Only the configured application relay can receive microphone audio. */
export function connectRelay(
  path: string,
  onAudio: (event: VoiceEvent) => void,
  onEvent: (event: RelayEvent) => void,
  signal: AbortSignal,
): Promise<RelayTransport> {
  let url: URL;
  try {
    url = relayUrl(path);
  } catch {
    return Promise.reject(new Error("The voice relay address is invalid."));
  }
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(new Error("Voice connection cancelled."));
      return;
    }
    const socket = new WebSocket(url);
    let ready = false;
    let terminal = false;
    let latest: RelayState | undefined;
    const timer = window.setTimeout(
      () => fail("Voice connection timed out."),
      30000,
    );
    function send(message: unknown) {
      if (socket.readyState !== WebSocket.OPEN)
        throw new Error("Voice connection is unavailable.");
      socket.send(JSON.stringify(message));
    }
    function cleanup() {
      window.clearTimeout(timer);
      signal.removeEventListener("abort", cancel);
    }
    function close() {
      cleanup();
      terminal = true;
      socket.close();
    }
    function cancel() {
      if (socket.readyState === WebSocket.OPEN)
        send({ type: "stop", request_id: crypto.randomUUID() });
      close();
      reject(new Error("Voice connection cancelled."));
    }
    function fail(message: string) {
      if (terminal) return;
      if (ready) {
        onAudio({ type: "disconnect" });
        onEvent({ type: "error", code: "connection_lost" });
      }
      close();
      reject(new Error(message));
    }
    signal.addEventListener("abort", cancel, { once: true });
    socket.onerror = () => fail("Voice connection failed.");
    socket.onclose = () => {
      if (!terminal) fail("Voice connection ended unexpectedly.");
    };
    socket.onmessage = (message) => {
      if (terminal) return;
      try {
        const event = JSON.parse(String(message.data));
        switch (event.type) {
          case "ready":
            latest = event;
            if (!ready) {
              ready = true;
              window.clearTimeout(timer);
              resolve({
                sendAudio(pcm) {
                  if (socket.bufferedAmount > 512 * 1024) {
                    fail("Voice connection could not keep up.");
                    throw new Error("Voice connection could not keep up.");
                  }
                  const bytes = new Uint8Array(pcm);
                  let binary = "";
                  for (const byte of bytes) binary += String.fromCharCode(byte);
                  send({ type: "audio", audio: btoa(binary) });
                },
                stop() {
                  if (socket.readyState === WebSocket.OPEN)
                    send({ type: "stop", request_id: crypto.randomUUID() });
                },
                close,
                control(action) {
                  if (!latest) throw new Error("Voice session is not ready.");
                  if (action === "finish" && latest.can_finish !== true)
                    throw new Error(
                      "The interview is not ready to finish yet.",
                    );
                  send({
                    type: "control",
                    request_id: crypto.randomUUID(),
                    action,
                    expected_revision: latest.revision,
                    expected_progress_revision: latest.progress_revision,
                  });
                },
              });
            }
            onEvent(event);
            break;
          case "state":
            latest = event;
            onEvent(event);
            break;
          case "control_rejected":
          case "caption":
            onEvent(event);
            break;
          case "reply_started":
            onAudio({ type: "reply.started" });
            break;
          case "clear_playback":
            onAudio({ type: "reply.done", status: "interrupted" });
            break;
          case "audio":
            onAudio({ type: "reply.audio", data: event.audio });
            break;
          case "ended":
            onEvent(event);
            onAudio({ type: "session.ended" });
            close();
            if (!ready)
              reject(new Error("Voice session ended before it was ready."));
            break;
          case "error":
            onEvent(event);
            fail("Voice session could not continue.");
            break;
        }
      } catch {
        fail("Voice connection returned an invalid response.");
      }
    };
  });
}
