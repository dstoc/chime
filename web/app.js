import { authenticatePasskey, registerPasskey } from "/passkeys.js";
import {
  clearDeviceCredentials,
  getDeviceCredentials,
  saveDeviceCredentials,
} from "/device-token.js";

const setupSection = document.querySelector("#setup");
const adminSection = document.querySelector("#admin");
const deviceSection = document.querySelector("#device-setup");
const keysSection = document.querySelector("#keys");
const devicesSection = document.querySelector("#devices");
const setupForm = document.querySelector("#setup-form");
const setupUnavailable = document.querySelector("#setup-unavailable");
const addForm = document.querySelector("#add-form");
const createKeyForm = document.querySelector("#create-key-form");
const deviceForm = document.querySelector("#device-form");
const passkeyList = document.querySelector("#passkey-list");
const sendKeyList = document.querySelector("#send-key-list");
const deviceList = document.querySelector("#device-list");
const newKeyPanel = document.querySelector("#new-key");
const newKeyValue = document.querySelector("#new-key-value");
const message = document.querySelector("#message");
const serverStatus = document.querySelector("#server-status");
const notificationHelp = document.querySelector("#notification-help");
const deviceStatus = document.querySelector("#device-status");

async function api(path, body) {
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "omit",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || "Chime could not complete the request.");
  return result;
}

function setBusy(busy) {
  document.querySelectorAll("button, input").forEach((control) => {
    control.disabled = busy;
  });
}

function say(text) {
  message.textContent = text;
}

function cancellation(error) {
  return error?.name === "NotAllowedError" || error?.name === "AbortError";
}

function managementReady() {
  adminSection.hidden = false;
  deviceSection.hidden = false;
  keysSection.hidden = false;
  devicesSection.hidden = false;
  updateNotificationSupport();
  if ("serviceWorker" in navigator && "Notification" in window && Notification.permission === "granted") {
    navigator.serviceWorker.register("/sw.js", { type: "module" }).catch(() => {
      deviceStatus.textContent = "Chime could not start notification support in this browser. Reload the page or check browser settings.";
    });
  }
}

function renderPasskeys(items) {
  passkeyList.replaceChildren();
  if (items.length === 0) {
    const empty = document.createElement("li");
    empty.textContent = "No administrator passkeys are registered.";
    passkeyList.append(empty);
    return;
  }

  for (const item of items) {
    const row = document.createElement("li");
    const name = document.createElement("span");
    name.className = "passkey-name";
    name.textContent = item.name;
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "remove-button";
    remove.textContent = "Remove";
    remove.addEventListener("click", () => runAdmin({ kind: "remove_passkey", id: item.id }));
    row.append(name, remove);
    passkeyList.append(row);
  }
}

function renderSendKeys(items) {
  sendKeyList.replaceChildren();
  if (items.length === 0) {
    const empty = document.createElement("li");
    empty.textContent = "No send keys have been created.";
    sendKeyList.append(empty);
    return;
  }

  for (const item of items) {
    const row = document.createElement("li");
    const detail = document.createElement("span");
    detail.className = "record-name";
    const name = document.createElement("span");
    name.textContent = item.name;
    const meta = document.createElement("span");
    meta.className = "record-meta";
    meta.textContent = item.revoked_at ? "Revoked" : `Created ${displayDate(item.created_at)}`;
    detail.append(name, meta);
    row.append(detail);
    if (!item.revoked_at) {
      const revoke = document.createElement("button");
      revoke.type = "button";
      revoke.className = "revoke-button";
      revoke.textContent = "Revoke";
      revoke.addEventListener("click", () => runAdmin({ kind: "revoke_send_key", id: item.id }));
      row.append(revoke);
    }
    sendKeyList.append(row);
  }
}

function renderDevices(items) {
  deviceList.replaceChildren();
  if (items.length === 0) {
    const empty = document.createElement("li");
    empty.textContent = "No receiving devices are enrolled.";
    deviceList.append(empty);
    return;
  }

  for (const item of items) {
    const row = document.createElement("li");
    const detail = document.createElement("span");
    detail.className = "record-name";
    const name = document.createElement("span");
    name.textContent = item.name;
    const meta = document.createElement("span");
    meta.className = "record-meta";
    meta.textContent = `Added ${displayDate(item.created_at)}`;
    detail.append(name, meta);
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "remove-button";
    remove.textContent = "Remove";
    remove.addEventListener("click", () => removeDevice(item));
    row.append(detail, remove);
    deviceList.append(row);
  }
}

function displayDate(unixSeconds) {
  return new Date(unixSeconds * 1000).toLocaleDateString();
}

async function runAdmin(operation) {
  setBusy(true);
  say(operation.kind === "remove_passkey" || operation.kind === "remove_device" || operation.kind === "revoke_send_key"
    ? "Verify this change with a passkey…"
    : "Verify this operation with a passkey…");
  try {
    const challenge = await api("/api/admin/challenge", operation);
    const credential = await authenticatePasskey(challenge.options);
    const result = await api("/api/admin/finish", {
      request_id: challenge.request_id,
      credential,
    });
    switch (result.kind) {
      case "add_registration": {
        say("Verify again to register the new passkey…");
        const newCredential = await registerPasskey(result.options);
        const completed = await api("/api/admin/add/finish", {
          request_id: result.request_id,
          credential: newCredential,
        });
        renderPasskeys(completed.passkeys);
        say("Passkey added.");
        return true;
      }
      case "passkeys":
        renderPasskeys(result.passkeys);
        say(operation.kind === "remove_passkey" ? "Passkey removed." : "Passkey list updated.");
        return true;
      case "send_keys":
        renderSendKeys(result.keys);
        say(operation.kind === "revoke_send_key" ? "Send key revoked." : "Send key list updated.");
        return true;
      case "send_key_created":
        renderSendKeys(result.keys);
        newKeyValue.value = result.secret;
        newKeyValue.type = "password";
        document.querySelector("#show-key").textContent = "Show";
        newKeyPanel.hidden = false;
        say("Send key created. Copy it now; Chime cannot show it again.");
        return true;
      case "devices":
        renderDevices(result.devices);
        say("Receiving device list updated.");
        return true;
      case "device_enrolled":
        renderDevices(result.devices);
        try {
          await saveDeviceCredentials(result.device.id, result.token);
          deviceStatus.textContent = "This browser is enrolled. Its private device token is stored in this site's IndexedDB.";
          say("Browser enrolled. It can now receive text notifications.");
        } catch (error) {
          deviceStatus.textContent = "The browser enrolled, but Chime could not save its device token. Enroll again to rotate and save a new token.";
          say(error.message);
        }
        return true;
      default:
        throw new Error("Chime returned an unknown administration result.");
    }
  } catch (error) {
    say(cancellation(error)
      ? "Passkey prompt cancelled. Nothing changed; start a new action when ready."
      : error.message);
    return false;
  } finally {
    setBusy(false);
  }
}

async function removeDevice(item) {
  const local = await getDeviceCredentials().catch(() => null);
  const removed = await runAdmin({ kind: "remove_device", id: item.id });
  if (!removed || local?.device_id !== item.id) return;
  try {
    const registration = await navigator.serviceWorker.getRegistration("/");
    const subscription = await registration?.pushManager.getSubscription();
    if (subscription && !(await subscription.unsubscribe())) {
      throw new Error("The browser kept its push subscription.");
    }
    await clearDeviceCredentials();
    deviceStatus.textContent = "This browser was removed and its push subscription was disabled.";
    say("Device removed.");
  } catch (error) {
    await clearDeviceCredentials().catch(() => {});
    deviceStatus.textContent = "The server removed this device, but browser notification settings may still need attention.";
    say(error.message);
  }
}

function isIosDevice() {
  return /iPad|iPhone|iPod/.test(navigator.userAgent)
    || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
}

function isHomeScreenApp() {
  return window.matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;
}

function updateNotificationSupport() {
  const supported = window.isSecureContext
    && "Notification" in window
    && "serviceWorker" in navigator
    && "PushManager" in window
    && "indexedDB" in window;
  const button = document.querySelector("#enable-notifications");
  if (!supported) {
    button.disabled = true;
    notificationHelp.textContent = "This browser cannot receive Chime push notifications. Use a supported browser on HTTPS.";
    return;
  }
  if (isIosDevice() && !isHomeScreenApp()) {
    notificationHelp.textContent = "On iPhone or iPad, add Chime to the Home Screen and open that app before enabling notifications.";
  } else if (Notification.permission === "denied") {
    button.disabled = true;
    notificationHelp.textContent = "Notifications are blocked. Change this site's notification permission in browser settings, then reload Chime.";
  } else if (Notification.permission === "granted") {
    notificationHelp.textContent = "Notifications are allowed. Enroll this browser with a passkey to receive messages.";
  }
}

async function enableNotifications() {
  const name = String(new FormData(deviceForm).get("name") || "").trim();
  if (isIosDevice() && !isHomeScreenApp()) {
    deviceStatus.textContent = "Add Chime to the Home Screen, open it there, then choose Enable notifications again.";
    return;
  }
  if (Notification.permission === "denied") {
    deviceStatus.textContent = "Notifications are blocked. Change this site's permission in browser settings, then reload Chime.";
    return;
  }
  setBusy(true);
  deviceStatus.textContent = "Choose Allow when the browser asks to send notifications.";
  try {
    const permission = Notification.permission === "granted"
      ? "granted"
      : await Notification.requestPermission();
    if (permission !== "granted") {
      deviceStatus.textContent = permission === "denied"
        ? "Notifications were denied. Change this site's permission in browser settings to continue."
        : "Notification permission was not granted. Choose Enable notifications when ready.";
      return;
    }
    const registration = await navigator.serviceWorker.register("/sw.js", { type: "module" });
    const { public_key: publicKey } = await api("/api/push-key");
    const applicationServerKey = decodeBase64url(publicKey);
    const subscription = await registration.pushManager.getSubscription()
      || await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey });
    const completed = await runAdmin({
      kind: "enroll_device",
      name,
      subscription: subscription.toJSON(),
    });
    if (!completed) {
      deviceStatus.textContent = "Push is enabled in this browser, but enrollment needs passkey approval. Retry to finish.";
    }
  } catch (error) {
    deviceStatus.textContent = error.message;
  } finally {
    setBusy(false);
  }
}

function decodeBase64url(value) {
  const base64 = value.replaceAll("-", "+").replaceAll("_", "/");
  const padded = base64.padEnd(Math.ceil(base64.length / 4) * 4, "=");
  return Uint8Array.from(atob(padded), (character) => character.charCodeAt(0));
}

setupForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = new FormData(setupForm);
  const code = String(form.get("code") || "").trim();
  const name = String(form.get("name") || "").trim();
  setBusy(true);
  setupUnavailable.hidden = true;
  say("Preparing passkey registration…");
  try {
    const challenge = await api("/api/setup/start", { code, name });
    const credential = await registerPasskey(challenge.options);
    await api("/api/setup/finish", { request_id: challenge.request_id, credential });
    setupForm.reset();
    setupSection.hidden = true;
    serverStatus.textContent = "Administrator passkey registered.";
    managementReady();
    say("Setup complete. Create a send key or enroll a receiving device.");
  } catch (error) {
    if (cancellation(error)) {
      say("Registration cancelled. The setup code was used to start this prompt; stop Chime and run reset-admin to replace it.");
      setupUnavailable.hidden = false;
    } else {
      say(error.message);
      if (error.message.includes("reset-admin")) setupUnavailable.hidden = false;
    }
  } finally {
    setBusy(false);
  }
});

addForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const name = String(new FormData(addForm).get("name") || "").trim();
  runAdmin({ kind: "add_passkey", name }).then((completed) => {
    if (completed) addForm.reset();
  });
});

createKeyForm.addEventListener("submit", (event) => {
  event.preventDefault();
  const name = String(new FormData(createKeyForm).get("name") || "").trim();
  newKeyPanel.hidden = true;
  newKeyValue.value = "";
  runAdmin({ kind: "create_send_key", name }).then((created) => {
    if (created) createKeyForm.reset();
  });
});

deviceForm.addEventListener("submit", (event) => {
  event.preventDefault();
  enableNotifications();
});

document.querySelector("#list-passkeys").addEventListener("click", () => {
  runAdmin({ kind: "list_passkeys" });
});
document.querySelector("#list-keys").addEventListener("click", () => {
  runAdmin({ kind: "list_send_keys" });
});
document.querySelector("#list-devices").addEventListener("click", () => {
  runAdmin({ kind: "list_devices" });
});
document.querySelector("#show-key").addEventListener("click", (event) => {
  const showing = newKeyValue.type === "text";
  newKeyValue.type = showing ? "password" : "text";
  event.currentTarget.textContent = showing ? "Show" : "Hide";
});
document.querySelector("#copy-key").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(newKeyValue.value);
    say("Send key copied. Store it in a protected place now.");
  } catch {
    newKeyValue.type = "text";
    newKeyValue.select();
    say("Copy is unavailable. Select the key and copy it now.");
  }
});

async function initialize() {
  if (!window.PublicKeyCredential || !navigator.credentials) {
    serverStatus.textContent = "This browser does not support passkeys.";
    say("Open Chime in a browser with WebAuthn support to manage the service.");
    return;
  }
  try {
    const status = await api("/api/setup/status");
    if (status.setup_required) {
      setupSection.hidden = false;
      setupUnavailable.hidden = status.setup_available;
      serverStatus.textContent = status.setup_available ? "First-time setup is available." : "First-time setup needs an operator.";
      return;
    }
    serverStatus.textContent = "Chime is ready.";
    managementReady();
    const credentials = await getDeviceCredentials().catch(() => null);
    if (credentials) {
      deviceStatus.textContent = "This browser has a stored device token. Use View devices to check its server enrollment.";
    }
  } catch (error) {
    serverStatus.textContent = "Chime could not be reached.";
    say(error.message);
  }
}

initialize();
