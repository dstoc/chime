import { authenticatePasskey, registerPasskey } from "/passkeys.js";

const setupSection = document.querySelector("#setup");
const adminSection = document.querySelector("#admin");
const setupForm = document.querySelector("#setup-form");
const setupUnavailable = document.querySelector("#setup-unavailable");
const addForm = document.querySelector("#add-form");
const passkeyList = document.querySelector("#passkey-list");
const message = document.querySelector("#message");
const serverStatus = document.querySelector("#server-status");

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

async function runAdmin(operation) {
  setBusy(true);
  say(operation.kind === "remove_passkey" ? "Verify to remove this passkey…" : "Verify this passkey operation…");
  try {
    const challenge = await api("/api/admin/challenge", operation);
    const credential = await authenticatePasskey(challenge.options);
    const result = await api("/api/admin/finish", {
      request_id: challenge.request_id,
      credential,
    });
    if (result.kind === "add_registration") {
      say("Verify again to register the new passkey…");
      const newCredential = await registerPasskey(result.options);
      const completed = await api("/api/admin/add/finish", {
        request_id: result.request_id,
        credential: newCredential,
      });
      renderPasskeys(completed.passkeys);
      say("Passkey added.");
    } else {
      renderPasskeys(result.passkeys);
      say(operation.kind === "remove_passkey" ? "Passkey removed." : "Passkey list updated.");
    }
    return true;
  } catch (error) {
    say(cancellation(error)
      ? "Passkey prompt cancelled. Nothing changed; start a new action when ready."
      : error.message);
    return false;
  } finally {
    setBusy(false);
  }
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
    adminSection.hidden = false;
    serverStatus.textContent = "Administrator passkey registered.";
    say("Setup complete. View the passkey list or add another passkey.");
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

document.querySelector("#list-passkeys").addEventListener("click", () => {
  runAdmin({ kind: "list_passkeys" });
});

async function initialize() {
  if (!window.PublicKeyCredential || !navigator.credentials) {
    serverStatus.textContent = "This browser does not support passkeys.";
    say("Open Chime in a browser with WebAuthn support to manage administrator passkeys.");
    return;
  }
  try {
    const status = await api("/api/setup/status");
    if (status.setup_required) {
      setupSection.hidden = false;
      setupUnavailable.hidden = status.setup_available;
      serverStatus.textContent = status.setup_available ? "First-time setup is available." : "First-time setup needs an operator.";
    } else {
      adminSection.hidden = false;
      serverStatus.textContent = "Chime is ready.";
    }
  } catch (error) {
    serverStatus.textContent = "Chime could not be reached.";
    say(error.message);
  }
}

initialize();
