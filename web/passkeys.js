function decodeBase64url(value) {
  const base64 = value.replaceAll("-", "+").replaceAll("_", "/");
  const padded = base64.padEnd(Math.ceil(base64.length / 4) * 4, "=");
  return Uint8Array.from(atob(padded), (character) => character.charCodeAt(0)).buffer;
}

function encodeBase64url(value) {
  const bytes = new Uint8Array(value);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

function convertOptions(options) {
  const publicKey = structuredClone(options.publicKey);
  publicKey.challenge = decodeBase64url(publicKey.challenge);
  if (publicKey.user) publicKey.user.id = decodeBase64url(publicKey.user.id);
  for (const descriptor of publicKey.allowCredentials ?? []) {
    descriptor.id = decodeBase64url(descriptor.id);
  }
  for (const descriptor of publicKey.excludeCredentials ?? []) {
    descriptor.id = decodeBase64url(descriptor.id);
  }
  return publicKey;
}

function credentialToJson(credential) {
  const response = credential.response;
  const json = {
    id: credential.id,
    rawId: encodeBase64url(credential.rawId),
    type: credential.type,
    response: {
      clientDataJSON: encodeBase64url(response.clientDataJSON),
    },
    clientExtensionResults: credential.getClientExtensionResults(),
  };

  if (response.attestationObject) {
    json.response.attestationObject = encodeBase64url(response.attestationObject);
    json.response.transports = response.getTransports?.() ?? [];
  }
  if (response.authenticatorData) {
    json.response.authenticatorData = encodeBase64url(response.authenticatorData);
    json.response.signature = encodeBase64url(response.signature);
    if (response.userHandle) json.response.userHandle = encodeBase64url(response.userHandle);
  }
  return json;
}

export async function registerPasskey(options) {
  const credential = await navigator.credentials.create({ publicKey: convertOptions(options) });
  if (!credential) throw new Error("Passkey registration was cancelled.");
  return credentialToJson(credential);
}

export async function authenticatePasskey(options) {
  const credential = await navigator.credentials.get({ publicKey: convertOptions(options) });
  if (!credential) throw new Error("Passkey verification was cancelled.");
  return credentialToJson(credential);
}
