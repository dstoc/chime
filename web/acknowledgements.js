import { getDeviceCredentials } from "/device-token.js";

const MAX_BATCH = 100;
const MAX_EVENT_BUFFER = 4096;
const MIN_RETRY_MS = 1000;
const MAX_RETRY_MS = 15000;

let activeController;

export function startAcknowledgementSync() {
  activeController?.abort();
  activeController = new AbortController();
  const controller = activeController;
  const { signal } = controller;
  window.addEventListener("pagehide", () => controller.abort(), { once: true });
  synchronize(signal).catch(() => {});
}

export function stopAcknowledgementSync() {
  activeController?.abort();
  activeController = undefined;
}

async function synchronize(signal) {
  let retryMs = MIN_RETRY_MS;
  while (!signal.aborted) {
    const credentials = await getDeviceCredentials().catch(() => null);
    if (!credentials?.token) return;
    try {
      const registration = await navigator.serviceWorker.getRegistration("/");
      if (!registration) return;
      const response = await fetch("/api/device/events", {
        headers: { Authorization: `Bearer ${credentials.token}` },
        cache: "no-store",
        credentials: "same-origin",
        signal,
      });
      if (response.status === 401 || response.status === 403) return;
      if (!response.ok || !response.body) throw new Error("The event stream is unavailable.");
      retryMs = MIN_RETRY_MS;
      await reconcileVisible(registration, credentials.token, signal);
      await readEvents(response.body, registration, signal);
    } catch {
      if (signal.aborted) return;
    }
    await delay(retryMs, signal);
    retryMs = Math.min(retryMs * 2, MAX_RETRY_MS);
  }
}

export async function reconcileVisible(registration, token, signal = new AbortController().signal) {
  const notifications = await registration.getNotifications();
  const ids = [...new Set(notifications
    .map((notification) => notification.data?.id)
    .filter(validId))];

  for (let offset = 0; offset < ids.length; offset += MAX_BATCH) {
    const response = await fetch("/api/device/reconcile", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ ids: ids.slice(offset, offset + MAX_BATCH) }),
      cache: "no-store",
      credentials: "same-origin",
      signal,
    });
    if (!response.ok) throw new Error("Acknowledgement reconciliation failed.");
    const result = await response.json();
    if (!Array.isArray(result.acknowledged)) throw new Error("The reconciliation response is invalid.");
    for (const id of result.acknowledged) {
      if (validId(id) && ids.includes(id)) await closeMatching(registration, id);
    }
  }
}

export async function readEvents(body, registration, signal = new AbortController().signal) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (!signal.aborted) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer += decoder.decode(value, { stream: true });
      if (new TextEncoder().encode(buffer).length > MAX_EVENT_BUFFER) {
        throw new Error("The event stream buffer limit was exceeded.");
      }
      let boundary;
      while ((boundary = buffer.indexOf("\n\n")) !== -1) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        await handleEventFrame(frame, registration);
      }
    }
  } finally {
    reader.releaseLock();
  }
}

export async function handleEventFrame(frame, registration) {
  let eventType = "message";
  let data = "";
  for (const line of frame.split("\n")) {
    if (line.startsWith("event:")) eventType = line.slice(6).trim();
    if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  if (eventType !== "acknowledged" || data.length > 64) return;
  try {
    const event = JSON.parse(data);
    if (validId(event.id)) await closeMatching(registration, event.id);
  } catch {
    // Ignore malformed events. The server emits only fixed, bounded frames.
  }
}

async function closeMatching(registration, id) {
  const notifications = await registration.getNotifications({ tag: `chime-${id}` });
  for (const notification of notifications) {
    if (notification.data?.id === id) notification.close();
  }
}

function validId(value) {
  return typeof value === "string" && /^[A-Za-z0-9_-]{22}$/.test(value);
}

function delay(milliseconds, signal) {
  return new Promise((resolve) => {
    const finish = () => {
      signal.removeEventListener("abort", abort);
      resolve();
    };
    const abort = () => {
      clearTimeout(timer);
      finish();
    };
    const timer = setTimeout(finish, milliseconds);
    signal.addEventListener("abort", abort, { once: true });
  });
}
