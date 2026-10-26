const DATABASE_NAME = "chime-device";
const STORE_NAME = "credentials";
const DISMISSED_STORE_NAME = "dismissed";
const MAX_DISMISSED_NOTIFICATIONS = 4096;

function openDatabase() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DATABASE_NAME, 2);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(STORE_NAME)) {
        database.createObjectStore(STORE_NAME, { keyPath: "key" });
      }
      if (!database.objectStoreNames.contains(DISMISSED_STORE_NAME)) {
        database.createObjectStore(DISMISSED_STORE_NAME, { keyPath: "id" });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new Error("Chime could not open this browser's private device storage."));
  });
}

export async function saveDeviceCredentials(deviceId, token) {
  const database = await openDatabase();
  try {
    await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, "readwrite");
      transaction.objectStore(STORE_NAME).put({ key: "current", device_id: deviceId, token });
      transaction.oncomplete = resolve;
      transaction.onerror = () => reject(new Error("Chime could not save this device token."));
      transaction.onabort = () => reject(new Error("Chime could not save this device token."));
    });
  } finally {
    database.close();
  }
}

export async function getDeviceCredentials() {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, "readonly");
      const request = transaction.objectStore(STORE_NAME).get("current");
      request.onsuccess = () => resolve(request.result ?? null);
      request.onerror = () => reject(new Error("Chime could not read this browser's private device storage."));
    });
  } finally {
    database.close();
  }
}

export async function clearDeviceCredentials() {
  const database = await openDatabase();
  try {
    await new Promise((resolve, reject) => {
      const transaction = database.transaction([STORE_NAME, DISMISSED_STORE_NAME], "readwrite");
      transaction.objectStore(STORE_NAME).delete("current");
      transaction.objectStore(DISMISSED_STORE_NAME).clear();
      transaction.oncomplete = resolve;
      transaction.onerror = () => reject(new Error("Chime could not clear this browser's device token."));
      transaction.onabort = () => reject(new Error("Chime could not clear this browser's device token."));
    });
  } finally {
    database.close();
  }
}

export async function rememberDismissedNotification(id, expiresAt, now = Math.floor(Date.now() / 1000)) {
  if (!validNotificationId(id) || !Number.isSafeInteger(expiresAt) || expiresAt <= now) return;
  const database = await openDatabase();
  try {
    await new Promise((resolve, reject) => {
      const transaction = database.transaction(DISMISSED_STORE_NAME, "readwrite");
      const store = transaction.objectStore(DISMISSED_STORE_NAME);
      const request = store.getAll();
      request.onsuccess = () => {
        const entries = request.result;
        for (const entry of entries) {
          if (entry.expires_at <= now) store.delete(entry.id);
        }
        const live = entries.filter((entry) => entry.expires_at > now && entry.id !== id);
        live.sort((left, right) => left.expires_at - right.expires_at);
        while (live.length >= MAX_DISMISSED_NOTIFICATIONS) {
          store.delete(live.shift().id);
        }
        store.put({ id, expires_at: expiresAt });
      };
      request.onerror = () => transaction.abort();
      transaction.oncomplete = resolve;
      transaction.onerror = () => reject(new Error("Chime could not save notification dismissal state."));
      transaction.onabort = () => reject(new Error("Chime could not save notification dismissal state."));
    });
  } finally {
    database.close();
  }
}

export async function isNotificationDismissed(id, now = Math.floor(Date.now() / 1000)) {
  if (!validNotificationId(id)) return false;
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(DISMISSED_STORE_NAME, "readwrite");
      const store = transaction.objectStore(DISMISSED_STORE_NAME);
      const request = store.get(id);
      let dismissed = false;
      request.onsuccess = () => {
        const entry = request.result;
        if (entry?.expires_at > now) dismissed = true;
        else if (entry) store.delete(id);
      };
      request.onerror = () => transaction.abort();
      transaction.oncomplete = () => resolve(dismissed);
      transaction.onerror = () => reject(new Error("Chime could not read notification dismissal state."));
      transaction.onabort = () => reject(new Error("Chime could not read notification dismissal state."));
    });
  } finally {
    database.close();
  }
}

function validNotificationId(id) {
  return typeof id === "string" && /^[A-Za-z0-9_-]{22}$/.test(id);
}
