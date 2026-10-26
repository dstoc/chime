const DATABASE_NAME = "chime-device";
const STORE_NAME = "credentials";

function openDatabase() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DATABASE_NAME, 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore(STORE_NAME, { keyPath: "key" });
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
      const transaction = database.transaction(STORE_NAME, "readwrite");
      transaction.objectStore(STORE_NAME).delete("current");
      transaction.oncomplete = resolve;
      transaction.onerror = () => reject(new Error("Chime could not clear this browser's device token."));
      transaction.onabort = () => reject(new Error("Chime could not clear this browser's device token."));
    });
  } finally {
    database.close();
  }
}
