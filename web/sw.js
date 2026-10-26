import { getDeviceCredentials } from "/device-token.js";

const MAX_TITLE_BYTES = 200;
const MAX_BODY_BYTES = 1500;
const MAX_AGE_SECONDS = 60 * 60 + 60;
const ALLOWED_FIELDS = ["ack_capability", "body", "expires_at", "id", "image_url", "title", "url", "version"];

self.addEventListener("push", (event) => {
  let payload;
  try {
    payload = event.data?.json();
  } catch {
    payload = null;
  }

  const valid = validatePayload(payload);
  event.waitUntil((async () => {
    let deviceId = null;
    if (valid) {
      try {
        deviceId = (await getDeviceCredentials())?.device_id ?? null;
      } catch {
        // A missing token must not suppress a user-visible push.
      }
    }
    await displayPayload(self.registration, payload, deviceId);
  })());
});

self.addEventListener("notificationclick", (event) => {
  event.waitUntil(handleNotificationClick(
    event.notification,
    (url) => self.clients.openWindow(url),
  ));
});

export function openNotificationUrl(notification, openWindow) {
  const url = notification.data?.url;
  notification.close();
  if (typeof url === "string" && validHttpsUrl(url)) return openWindow(url);
  return null;
}

export function handleNotificationClick(notification, openWindow, acknowledge = acknowledgeNotification) {
  const opening = openNotificationUrl(notification, openWindow);
  const acknowledgement = acknowledge(notification.data);
  return Promise.allSettled([opening, acknowledgement]);
}

export async function acknowledgeNotification(data) {
  if (!data || !/^[A-Za-z0-9_-]{22}$/.test(data.id || "")
      || !/^[A-Za-z0-9_-]{43}$/.test(data.ack_capability || "")) return;
  try {
    const credentials = await getDeviceCredentials();
    if (!credentials?.token) return;
    await fetch("/api/device/ack", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${credentials.token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ id: data.id, capability: data.ack_capability }),
      cache: "no-store",
      credentials: "same-origin",
    });
  } catch {
    // A click always closes locally. Failed acknowledgements are not queued.
  }
}

export async function displayPayload(registration, payload, deviceId = null) {
  const valid = validatePayload(payload);
  const title = valid ? payload.title : "Chime";
  const options = valid
    ? {
        body: payload.body || "",
        tag: `chime-${payload.id}`,
        data: {
          id: payload.id,
          ack_capability: payload.ack_capability,
          device_id: deviceId,
          url: payload.url || null,
        },
      }
    : { body: "A notification could not be displayed." };
  if (valid && payload.image_url) options.image = payload.image_url;
  try {
    await registration.showNotification(title, options);
  } catch (error) {
    if (!valid || !payload.image_url) throw error;
    delete options.image;
    await registration.showNotification(title, options);
  }
}

export function validatePayload(payload) {
  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) return false;
  if (Object.keys(payload).some((field) => !ALLOWED_FIELDS.includes(field))) return false;
  if (payload.version !== 1 || typeof payload.title !== "string") return false;
  if (new TextEncoder().encode(payload.title).length > MAX_TITLE_BYTES || payload.title.trim() === "") return false;
  if (payload.body !== null && payload.body !== undefined && typeof payload.body !== "string") return false;
  if (typeof payload.body === "string" && new TextEncoder().encode(payload.body).length > MAX_BODY_BYTES) return false;
  if (payload.image_url !== null && payload.image_url !== undefined && !validHttpsUrl(payload.image_url)) return false;
  if (payload.url !== null && payload.url !== undefined && !validHttpsUrl(payload.url)) return false;
  if (!/^[A-Za-z0-9_-]{22}$/.test(payload.id)) return false;
  if (!/^[A-Za-z0-9_-]{43}$/.test(payload.ack_capability)) return false;
  if (!Number.isSafeInteger(payload.expires_at)) return false;
  const now = Math.floor(Date.now() / 1000);
  return payload.expires_at > now && payload.expires_at <= now + MAX_AGE_SECONDS;
}

function validHttpsUrl(value) {
  if (typeof value !== "string" || new TextEncoder().encode(value).length > 2048) return false;
  try {
    const url = new URL(value);
    return url.protocol === "https:" && Boolean(url.hostname) && !url.username && !url.password;
  } catch {
    return false;
  }
}

