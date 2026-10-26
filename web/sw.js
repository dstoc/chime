import { getDeviceCredentials } from "/device-token.js";

const MAX_TITLE_BYTES = 200;
const MAX_BODY_BYTES = 1500;
const MAX_AGE_SECONDS = 60 * 60 + 60;
const ALLOWED_FIELDS = ["ack_capability", "body", "expires_at", "id", "title", "version"];

self.addEventListener("push", (event) => {
  let payload;
  try {
    payload = event.data?.json();
  } catch {
    payload = null;
  }

  const valid = validatePayload(payload);
  const title = valid ? payload.title : "Chime";
  const body = valid ? payload.body || "" : "A notification could not be displayed.";
  event.waitUntil((async () => {
    let deviceId = null;
    if (valid) {
      try {
        deviceId = (await getDeviceCredentials())?.device_id ?? null;
      } catch {
        // A missing token must not suppress a user-visible push.
      }
    }
    const options = valid
      ? {
          body,
          tag: `chime-${payload.id}`,
          data: { id: payload.id, ack_capability: payload.ack_capability, device_id: deviceId },
        }
      : { body };
    await self.registration.showNotification(title, options);
  })());
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
});

export function validatePayload(payload) {
  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) return false;
  if (Object.keys(payload).some((field) => !ALLOWED_FIELDS.includes(field))) return false;
  if (payload.version !== 1 || typeof payload.title !== "string") return false;
  if (new TextEncoder().encode(payload.title).length > MAX_TITLE_BYTES || payload.title.trim() === "") return false;
  if (payload.body !== null && payload.body !== undefined && typeof payload.body !== "string") return false;
  if (typeof payload.body === "string" && new TextEncoder().encode(payload.body).length > MAX_BODY_BYTES) return false;
  if (!/^[A-Za-z0-9_-]{22}$/.test(payload.id)) return false;
  if (!/^[A-Za-z0-9_-]{43}$/.test(payload.ack_capability)) return false;
  if (!Number.isSafeInteger(payload.expires_at)) return false;
  const now = Math.floor(Date.now() / 1000);
  return payload.expires_at > now && payload.expires_at <= now + MAX_AGE_SECONDS;
}

