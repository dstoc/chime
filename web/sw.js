import {
  getDeviceCredentials,
  isNotificationDismissed,
  rememberDismissedNotification,
} from "/device-token.js";

const MAX_TITLE_BYTES = 200;
const MAX_BODY_BYTES = 1500;
const MAX_AGE_SECONDS = 60 * 60 + 60;
const NOTIFY_FIELDS = ["ack_capability", "body", "expires_at", "id", "image_url", "title", "type", "url", "version"];
const DISMISS_FIELDS = ["expires_at", "id", "type", "version"];
let pushQueue = Promise.resolve();

self.addEventListener("push", (event) => {
  let payload;
  try {
    payload = event.data?.json();
  } catch {
    payload = null;
  }
  event.waitUntil(enqueuePush(() => handlePushPayload(self.registration, payload)));
});

self.addEventListener("notificationclick", (event) => {
  event.waitUntil(handleNotificationClick(
    event.notification,
    (url) => self.clients.openWindow(url),
    acknowledgeNotification,
    self.registration,
  ));
});

export function enqueuePush(work) {
  const result = pushQueue.then(work);
  pushQueue = result.catch(() => {});
  return result;
}

export async function handlePushPayload(registration, payload, rememberDismissed = rememberDismissedNotification) {
  if (payload?.type === "dismiss") {
    if (!validateDismissPayload(payload)) return false;
    await Promise.allSettled([rememberDismissed(payload.id, payload.expires_at)]);
    const notifications = await registration.getNotifications({ tag: `chime-${payload.id}` });
    for (const notification of notifications) notification.close();
    return true;
  }
  return displayPayload(registration, payload);
}

export function openNotificationUrl(notification, openWindow) {
  const url = notification.data?.url;
  notification.close();
  if (typeof url === "string" && validHttpsUrl(url)) return openWindow(url);
  return null;
}

export function handleNotificationClick(
  notification,
  openWindow,
  acknowledge = acknowledgeNotification,
  registration = self.registration,
) {
  const opening = openNotificationUrl(notification, openWindow);
  const data = notification.data ?? {};
  const localWork = enqueuePush(async () => {
    try {
      await rememberDismissedNotification(data.id, data.expires_at);
    } catch {
      // Local dismissal must still close a notification if IndexedDB is unavailable.
    }
    if (!validId(data.id)) return;
    const notifications = await registration.getNotifications({ tag: `chime-${data.id}` });
    for (const item of notifications) item.close();
  });
  const acknowledgement = Promise.resolve().then(() => acknowledge(data));
  return Promise.allSettled([opening, localWork, acknowledgement]);
}

export async function acknowledgeNotification(data) {
  if (!data || !validId(data.id) || !validCapability(data.ack_capability)) return;
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

export async function displayPayload(registration, payload, hasDismissed = isNotificationDismissed) {
  const valid = validateNotifyPayload(payload);
  if (valid) {
    try {
      if (await hasDismissed(payload.id)) return false;
    } catch {
      // Dismissal memory must not suppress an ordinary notification.
    }
  }
  const title = valid ? payload.title : "Chime";
  const options = valid
    ? {
        body: payload.body || "",
        tag: `chime-${payload.id}`,
        data: {
          id: payload.id,
          ack_capability: payload.ack_capability,
          expires_at: payload.expires_at,
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
  return true;
}

export function validatePayload(payload) {
  return validateNotifyPayload(payload) || validateDismissPayload(payload);
}

export function validateNotifyPayload(payload) {
  if (!isPlainObject(payload) || payload.type !== "notify") return false;
  if (Object.keys(payload).some((field) => !NOTIFY_FIELDS.includes(field))) return false;
  if (payload.version !== 1 || typeof payload.title !== "string") return false;
  if (new TextEncoder().encode(payload.title).length > MAX_TITLE_BYTES || payload.title.trim() === "") return false;
  if (payload.body !== null && payload.body !== undefined && typeof payload.body !== "string") return false;
  if (typeof payload.body === "string" && new TextEncoder().encode(payload.body).length > MAX_BODY_BYTES) return false;
  if (payload.image_url !== null && payload.image_url !== undefined && !validHttpsUrl(payload.image_url)) return false;
  if (payload.url !== null && payload.url !== undefined && !validHttpsUrl(payload.url)) return false;
  if (!validId(payload.id) || !validCapability(payload.ack_capability)) return false;
  return validExpiry(payload.expires_at);
}

export function validateDismissPayload(payload) {
  return isPlainObject(payload)
    && payload.type === "dismiss"
    && Object.keys(payload).every((field) => DISMISS_FIELDS.includes(field))
    && payload.version === 1
    && validId(payload.id)
    && validExpiry(payload.expires_at);
}

function validExpiry(value) {
  if (!Number.isSafeInteger(value)) return false;
  const now = Math.floor(Date.now() / 1000);
  return value > now && value <= now + MAX_AGE_SECONDS;
}

function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function validId(value) {
  return typeof value === "string" && /^[A-Za-z0-9_-]{22}$/.test(value);
}

function validCapability(value) {
  return typeof value === "string" && /^[A-Za-z0-9_-]{43}$/.test(value);
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
