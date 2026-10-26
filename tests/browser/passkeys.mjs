import { createHash, createPrivateKey, generateKeyPairSync, sign } from 'node:crypto';
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const serverBinary = process.env.CHIME_SERVER_BIN || path.join(repoRoot, 'target/debug/chime-server');
const chromiumPath = process.env.CHROMIUM_PATH || '/usr/bin/chromium';
if (!fs.existsSync(serverBinary)) throw new Error(`server binary not found: ${serverBinary}; run cargo build -p chime-server first`);
if (!fs.existsSync(chromiumPath)) throw new Error(`Chromium not found: ${chromiumPath}; set CHROMIUM_PATH if needed`);

const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'chime-passkeys-'));
const databasePath = path.join(workspace, 'data', 'chime.sqlite3');
let service;
let browser;
let proxy;
let socket;
let authenticatorId;
const serviceOutput = [];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function freePort() {
  const listener = net.createServer();
  await new Promise((resolve, reject) => {
    listener.once('error', reject);
    listener.listen(0, '127.0.0.1', resolve);
  });
  const port = listener.address().port;
  await new Promise((resolve, reject) => listener.close((error) => error ? reject(error) : resolve()));
  return port;
}

async function waitForBrowserPort(profile) {
  const activePortFile = path.join(profile, 'DevToolsActivePort');
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (fs.existsSync(activePortFile)) {
      const port = Number(fs.readFileSync(activePortFile, 'utf8').split('\n')[0]);
      if (Number.isInteger(port) && port > 0) return port;
    }
    if (browser.exitCode !== null) throw new Error(`Chromium exited with ${browser.exitCode}`);
    await sleep(100);
  }
  throw new Error('Chromium did not open its DevTools port');
}

async function waitForSetupCode() {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Chime did not print its first-use setup code')), 10000);
    const inspect = (chunk) => {
      const match = chunk.toString().match(/First-use setup code \(sensitive; save it now\): ([A-Za-z0-9_-]+)/);
      if (match) {
        clearTimeout(timer);
        resolve(match[1]);
      }
    };
    service.stdout.on('data', inspect);
    service.stderr.on('data', inspect);
    service.once('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
    service.once('exit', (status) => {
      clearTimeout(timer);
      reject(new Error(`Chime exited before setup: ${status}`));
    });
  });
}

async function stopProcess(child, signal = 'SIGTERM') {
  if (!child || child.exitCode !== null) return;
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      resolve();
    }, 2000);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve();
    });
    child.kill(signal);
  });
}

try {
const proxyPort = await freePort();
const devtoolsPort = 0;
const origin = `https://localhost:${proxyPort}`;
const certPath = path.join(workspace, 'cert.pem');
const keyPath = path.join(workspace, 'key.pem');
execFileSync('openssl', [
  'req', '-x509', '-newkey', 'rsa:2048', '-keyout', keyPath, '-out', certPath,
  '-sha256', '-days', '1', '-nodes', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost',
], { stdio: 'ignore' });

proxy = https.createServer({ key: fs.readFileSync(keyPath), cert: fs.readFileSync(certPath) }, (request, response) => {
  const upstream = http.request({
    host: '127.0.0.1', port: 8080, method: request.method, path: request.url,
    headers: { ...request.headers, host: '127.0.0.1:8080' },
  }, (upstreamResponse) => {
    response.writeHead(upstreamResponse.statusCode, upstreamResponse.headers);
    upstreamResponse.pipe(response);
  });
  upstream.on('error', () => { response.writeHead(502); response.end(); });
  request.pipe(upstream);
});
await new Promise((resolve, reject) => {
  proxy.once('error', reject);
  proxy.listen(proxyPort, '127.0.0.1', resolve);
});

service = spawn(serverBinary, [
  '--origin', origin,
  '--data-dir', path.join(workspace, 'data'),
], { cwd: repoRoot, stdio: ['ignore', 'pipe', 'pipe'] });
service.stdout.setEncoding('utf8');
service.stderr.setEncoding('utf8');
service.stdout.on('data', (text) => serviceOutput.push(text));
service.stderr.on('data', (text) => serviceOutput.push(text));
const code = await waitForSetupCode();

const browserProfile = path.join(workspace, 'browser');
browser = spawn(chromiumPath, [
  '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
  '--ignore-certificate-errors', `--remote-debugging-port=${devtoolsPort}`,
  `--user-data-dir=${browserProfile}`, '--no-first-run', '--no-default-browser-check', 'about:blank',
], { stdio: 'ignore' });
const devtoolsPortNumber = await waitForBrowserPort(browserProfile);
const chromeVersion = execFileSync(chromiumPath, ['--version'], { encoding: 'utf8' }).trim();
const targetResponse = await fetch(`http://127.0.0.1:${devtoolsPortNumber}/json/new?${origin}`, { method: 'PUT' });
if (!targetResponse.ok) throw new Error(`new page failed: ${targetResponse.status}`);
const target = await targetResponse.json();
socket = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true });
  socket.addEventListener('error', reject, { once: true });
});
let nextId = 0;
const pending = new Map();
let serviceWorkerRegistrationId = null;
socket.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);
  if (message.method === 'ServiceWorker.workerRegistrationUpdated') {
    const registration = message.params.registrations.find((entry) => entry.scopeURL === `${origin}/` && !entry.isDeleted);
    if (registration) serviceWorkerRegistrationId = registration.registrationId;
  }
  if (message.id && pending.has(message.id)) {
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(new Error(message.error.message));
    else resolve(message.result);
  }
});
function cdp(method, params = {}) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params }));
  });
}
async function evaluate(expression) {
  const result = await cdp('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
    userGesture: true,
  });
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.text);
  return result.result.value;
}
async function waitFor(predicate, timeoutMs = 10000) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    const state = await evaluate(`(() => ({ status: document.querySelector('#server-status')?.textContent, message: document.querySelector('#message')?.textContent, setupHidden: document.querySelector('#setup')?.hidden, adminHidden: document.querySelector('#admin')?.hidden }))()`);
    if (predicate(state)) return state;
    await sleep(150);
  }
  throw new Error('timed out waiting for page state');
}
function sqliteQuery(statement, params = []) {
  const script = 'import json, sqlite3, sys; request=json.loads(sys.stdin.read()); db=sqlite3.connect(sys.argv[1]); rows=db.execute(request["statement"], request["params"]).fetchall(); print(json.dumps(rows)); db.close()';
  const output = execFileSync('python3', ['-c', script, databasePath], {
    input: JSON.stringify({ statement, params }),
    encoding: 'utf8',
  });
  return JSON.parse(output);
}
function persistentFilesContain(value) {
  return ['chime.sqlite3', 'chime.sqlite3-wal', 'chime.sqlite3-shm']
    .map((name) => path.join(workspace, 'data', name))
    .filter((file) => fs.existsSync(file))
    .some((file) => fs.readFileSync(file).includes(Buffer.from(value)));
}
await cdp('Page.enable');
await cdp('Runtime.enable');
await cdp('WebAuthn.enable', { enableUI: false });
authenticatorId = (await cdp('WebAuthn.addVirtualAuthenticator', {
  options: {
    protocol: 'ctap2',
    transport: 'internal',
    hasResidentKey: true,
    hasUserVerification: true,
    isUserVerified: true,
    automaticPresenceSimulation: true,
  },
})).authenticatorId;
await cdp('Page.navigate', { url: origin });
await waitFor((state) => state.setupHidden === false);
const invalidSetup = await evaluate(`(async () => {
  const response = await fetch('/api/setup/start', { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify({code:'wrong-code',name:'QA key'}) });
  return response.status;
})()`);
if (invalidSetup !== 403) throw new Error(`invalid setup code status ${invalidSetup}`);
const setupResult = await evaluate(`(async () => {
  const originalFetch = window.fetch.bind(window);
  window.fetch = async (...args) => {
    const response = await originalFetch(...args);
    if (String(args[0]).includes('/api/setup/start') && response.ok) window.qaSetupChallenge = await response.clone().json();
    return response;
  };
  document.querySelector('#setup-code').value = ${JSON.stringify(code)};
  document.querySelector('#setup-name').value = 'QA primary';
  document.querySelector('#setup-form').requestSubmit();
  await new Promise(resolve => setTimeout(resolve, 1800));
  return { status: document.querySelector('#server-status').textContent, message: document.querySelector('#message').textContent, adminHidden: document.querySelector('#admin').hidden };
})()`);
if (setupResult.adminHidden || !setupResult.message.includes('Setup complete')) throw new Error(`setup failed: ${JSON.stringify(setupResult)}`);
const setupUv = await evaluate(`window.qaSetupChallenge.options.publicKey.authenticatorSelection.userVerification`);
if (setupUv !== 'required') throw new Error(`setup did not require user verification: ${setupUv}`);
const firstList = await evaluate(`(async () => {
  document.querySelector('#list-passkeys').click();
  await new Promise(resolve => setTimeout(resolve, 1000));
  return { message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length };
})()`);
if (firstList.count !== 1 || firstList.message !== 'Passkey list updated.') throw new Error(`list failed: ${JSON.stringify(firstList)}`);

async function createSendKeyInUi(name) {
  await evaluate(`(() => {
    document.querySelector('#key-name').value = ${JSON.stringify(name)};
    document.querySelector('#create-key-form').requestSubmit();
  })()`);
  await waitFor((state) => state.message.startsWith('Send key created.'));
  return evaluate(`(() => ({
    secret: document.querySelector('#new-key-value').value,
    keyPanelHidden: document.querySelector('#new-key').hidden,
    keyList: document.querySelector('#send-key-list').textContent,
  }))()`);
}

async function sendWithKey(key) {
  const authorization = JSON.stringify(`Bearer ${key}`);
  return evaluate(`(async () => {
    const response = await fetch('/api/notify', {
      method: 'POST',
      headers: { 'Authorization': ${authorization}, 'Content-Type': 'application/json' },
      body: JSON.stringify({ title: 'Local acceptance check' }),
    });
    let body = null;
    try { body = await response.json(); } catch {}
    return { status: response.status, body };
  })()`);
}

const firstSendKey = await createSendKeyInUi('QA sender one');
if (!firstSendKey.secret.startsWith('chime_sk_') || firstSendKey.keyPanelHidden) {
  throw new Error('the send-key UI did not show a new key once');
}
const storedSendKeyDigest = sqliteQuery('SELECT hex(secret_digest) FROM send_keys WHERE name = ?', ['QA sender one']);
const expectedSendKeyDigest = createHash('sha256').update(firstSendKey.secret).digest('hex').toUpperCase();
if (storedSendKeyDigest.length !== 1 || storedSendKeyDigest[0][0] !== expectedSendKeyDigest
  || persistentFilesContain(firstSendKey.secret) || serviceOutput.join('').includes(firstSendKey.secret)) {
  throw new Error('send-key plaintext was stored or logged instead of storing only its digest');
}
const firstSendKeyListed = await evaluate(`(async () => {
  document.querySelector('#list-keys').click();
  await new Promise(resolve => setTimeout(resolve, 900));
  const text = document.querySelector('#send-key-list').textContent;
  return { message: document.querySelector('#message').textContent, text };
})()`);
if (firstSendKeyListed.message !== 'Send key list updated.' || firstSendKeyListed.text.includes(firstSendKey.secret)) {
  throw new Error('send-key list did not return metadata only');
}
const noDevices = await sendWithKey(firstSendKey.secret);
if (noDevices.status !== 200 || noDevices.body.outcome !== 'no_devices') {
  throw new Error(`send without enrolled devices returned ${JSON.stringify(noDevices)}`);
}
await evaluate(`(() => document.querySelector('#send-key-list .revoke-button').click())()`);
const revokedKeyUi = await waitFor((state) => state.message === 'Send key revoked.');
const revokedKeySend = await sendWithKey(firstSendKey.secret);
if (revokedKeySend.status !== 401 || !revokedKeyUi.message.includes('revoked')) {
  throw new Error('revoked send key remained valid');
}
const secondSendKey = await createSendKeyInUi('QA sender two');
const secondKeyNoDevices = await sendWithKey(secondSendKey.secret);
if (!secondSendKey.secret.startsWith('chime_sk_') || secondKeyNoDevices.status !== 200 || secondKeyNoDevices.body.outcome !== 'no_devices') {
  throw new Error('revoking one send key affected an independent key');
}

const generatedSubscriptionKeys = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
const subscriptionPublicDer = generatedSubscriptionKeys.publicKey.export({ format: 'der', type: 'spki' });
const testSubscription = {
  endpoint: 'https://fcm.googleapis.com/fcm/send/local-test-token',
  expirationTime: null,
  keys: {
    p256dh: subscriptionPublicDer.subarray(-65).toString('base64url'),
    auth: Buffer.from('0123456789abcdef').toString('base64url'),
  },
};
const invalidSubscription = await evaluate(`(async () => {
  const response = await fetch('/api/admin/challenge', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({kind:'enroll_device', name:'Bad device', subscription:{...${JSON.stringify(testSubscription)}, endpoint:'https://127.0.0.1/private'}}),
  });
  return response.status;
})()`);
if (invalidSubscription !== 400) throw new Error(`unsupported provider endpoint returned ${invalidSubscription}`);

async function completeAdminOperation(operation) {
  const assertion = await browserAssertion(operation);
  return finishAt('/api/admin/finish', {
    request_id: assertion.requestId,
    credential: assertion.credential,
  });
}
const firstEnrollment = await completeAdminOperation({ kind: 'enroll_device', name: 'QA browser', subscription: testSubscription });
if (firstEnrollment.status !== 200 || firstEnrollment.body.kind !== 'device_enrolled') {
  throw new Error(`device enrollment failed: ${JSON.stringify(firstEnrollment)}`);
}
const secondEnrollment = await completeAdminOperation({ kind: 'enroll_device', name: 'QA browser renamed', subscription: testSubscription });
if (secondEnrollment.status !== 200
  || secondEnrollment.body.device.id !== firstEnrollment.body.device.id
  || secondEnrollment.body.token === firstEnrollment.body.token
  || secondEnrollment.body.devices.length !== 1) {
  throw new Error('duplicate endpoint enrollment did not upsert and rotate the token');
}
const tokenStorage = await evaluate(`(async () => {
  const tokens = await import('/device-token.js');
  await tokens.saveDeviceCredentials(${JSON.stringify(secondEnrollment.body.device.id)}, ${JSON.stringify(secondEnrollment.body.token)});
  return tokens.getDeviceCredentials();
})()`);
if (tokenStorage.device_id !== secondEnrollment.body.device.id || tokenStorage.token !== secondEnrollment.body.token) {
  throw new Error('device token was not stored in IndexedDB');
}
const storedDeviceTokenDigest = sqliteQuery('SELECT hex(token_digest) FROM receiving_devices WHERE id = ?', [secondEnrollment.body.device.id]);
const expectedDeviceTokenDigest = createHash('sha256').update(secondEnrollment.body.token).digest('hex').toUpperCase();
if (storedDeviceTokenDigest.length !== 1 || storedDeviceTokenDigest[0][0] !== expectedDeviceTokenDigest
  || persistentFilesContain(secondEnrollment.body.token) || serviceOutput.join('').includes(secondEnrollment.body.token)) {
  throw new Error('device token plaintext was stored or logged instead of storing only its digest');
}
const listedDevices = await completeAdminOperation({ kind: 'list_devices' });
if (listedDevices.status !== 200 || listedDevices.body.devices.length !== 1) {
  throw new Error('device list did not contain one upserted endpoint');
}
const removedDevice = await completeAdminOperation({ kind: 'remove_device', id: secondEnrollment.body.device.id });
if (removedDevice.status !== 200 || removedDevice.body.devices.length !== 0) {
  throw new Error('device removal did not invalidate its record');
}
const clearedToken = await evaluate(`(async () => {
  const tokens = await import('/device-token.js');
  await tokens.clearDeviceCredentials();
  return tokens.getDeviceCredentials();
})()`);
if (clearedToken !== null) throw new Error('removed device token remained in IndexedDB');

const payloadValidation = await evaluate(`(async () => {
  const { validatePayload } = await import('/sw.js');
  const valid = {
    version: 1, id: 'AAAAAAAAAAAAAAAAAAAAAA', title: 'Build complete', body: 'All checks passed',
    expires_at: Math.floor(Date.now() / 1000) + 3600, ack_capability: 'A'.repeat(43),
  };
  return {
    valid: validatePayload(valid),
    oldVersion: validatePayload({ ...valid, version: 2 }),
    expired: validatePayload({ ...valid, expires_at: 1 }),
    unsafeExtra: validatePayload({ ...valid, html: '<script>' }),
  };
})()`);
if (!payloadValidation.valid || payloadValidation.oldVersion || payloadValidation.expired || payloadValidation.unsafeExtra) {
  throw new Error(`service-worker payload validation failed: ${JSON.stringify(payloadValidation)}`);
}
await cdp('Browser.grantPermissions', { origin, permissions: ['notifications'] });
await cdp('ServiceWorker.enable');
await evaluate(`navigator.serviceWorker.register('/sw.js', { type: 'module' }).then(() => navigator.serviceWorker.ready)`);
for (let attempt = 0; attempt < 100 && !serviceWorkerRegistrationId; attempt += 1) await sleep(50);
if (!serviceWorkerRegistrationId) throw new Error('service worker did not register with Chromium');
const validPushPayload = {
  version: 1,
  id: 'AAAAAAAAAAAAAAAAAAAAAA',
  title: 'Build complete',
  body: 'All checks passed',
  expires_at: Math.floor(Date.now() / 1000) + 3600,
  ack_capability: 'A'.repeat(43),
};
await cdp('ServiceWorker.deliverPushMessage', {
  origin,
  registrationId: serviceWorkerRegistrationId,
  data: JSON.stringify(validPushPayload),
});
const displayedPush = await evaluate(`(async () => {
  const registration = await navigator.serviceWorker.ready;
  for (let attempt = 0; attempt < 60; attempt += 1) {
    const notifications = await registration.getNotifications({ tag: 'chime-AAAAAAAAAAAAAAAAAAAAAA' });
    if (notifications.length) return notifications.map((item) => ({ title: item.title, body: item.body, data: item.data }));
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return [];
})()`);
if (displayedPush.length !== 1 || displayedPush[0].title !== 'Build complete' || displayedPush[0].body !== 'All checks passed') {
  throw new Error(`valid push was not displayed as text: ${JSON.stringify(displayedPush)}`);
}
const invalidPushPayload = { ...validPushPayload, id: 'BBBBBBBBBBBBBBBBBBBBBB', version: 2 };
await cdp('ServiceWorker.deliverPushMessage', {
  origin,
  registrationId: serviceWorkerRegistrationId,
  data: JSON.stringify(invalidPushPayload),
});
const fallbackPush = await evaluate(`(async () => {
  const registration = await navigator.serviceWorker.ready;
  for (let attempt = 0; attempt < 60; attempt += 1) {
    const notifications = await registration.getNotifications();
    const fallback = notifications.find((item) => item.title === 'Chime');
    if (fallback) return { title: fallback.title, body: fallback.body };
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return null;
})()`);
if (!fallbackPush || fallbackPush.body !== 'A notification could not be displayed.') {
  throw new Error(`malformed push fallback was not displayed: ${JSON.stringify(fallbackPush)}`);
}
await evaluate(`(() => {
  window.qaOriginalCreate = navigator.credentials.create.bind(navigator.credentials);
  navigator.credentials.create = (options) => new Promise((resolve, reject) => {
    window.qaCreatePending = true;
    window.qaReleaseCreate = () => {
      window.qaCreatePending = false;
      delete window.qaReleaseCreate;
      window.qaOriginalCreate(options).then(resolve, reject);
    };
  });
})()`);
const addSecond = await evaluate(`(async () => {
  document.querySelector('#add-name').value = 'QA secondary';
  document.querySelector('#add-form').requestSubmit();
  const end = Date.now() + 8000;
  while (!window.qaCreatePending && Date.now() < end) await new Promise(resolve => setTimeout(resolve, 50));
  if (!window.qaCreatePending) throw new Error('admin authorization did not reach passkey registration');
  return { message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length };
})()`);
if (!addSecond.message.includes('register the new passkey')) throw new Error(`add authorization failed: ${JSON.stringify(addSecond)}`);
const originalCredentialState = await cdp('WebAuthn.getCredentials', { authenticatorId });
if (originalCredentialState.credentials.length !== 1) throw new Error('expected one credential in the initial virtual authenticator');
await cdp('WebAuthn.removeVirtualAuthenticator', { authenticatorId });
const addedAuthenticator = await cdp('WebAuthn.addVirtualAuthenticator', {
  options: {
    protocol: 'ctap2',
    transport: 'internal',
    hasResidentKey: true,
    hasUserVerification: true,
    isUserVerified: true,
    automaticPresenceSimulation: true,
  },
});
authenticatorId = addedAuthenticator.authenticatorId;
await evaluate('window.qaReleaseCreate()');
const addCompleted = await waitFor((state) => state.message === 'Passkey added.', 10000);
await cdp('WebAuthn.addCredential', {
  authenticatorId,
  credential: originalCredentialState.credentials[0],
});
await evaluate('navigator.credentials.create = window.qaOriginalCreate');
const addSecondResult = await evaluate(`(() => ({ message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length }))()`);
if (addSecondResult.count !== 2 || addCompleted.adminHidden) throw new Error(`add failed: ${JSON.stringify(addSecondResult)}`);
const cancelAdd = await evaluate(`(async () => {
  const original = navigator.credentials.create.bind(navigator.credentials);
  navigator.credentials.create = () => Promise.reject(new DOMException('cancelled', 'NotAllowedError'));
  document.querySelector('#add-name').value = 'Cancelled key';
  document.querySelector('#add-form').requestSubmit();
  await new Promise(resolve => setTimeout(resolve, 1200));
  const result = { message: document.querySelector('#message').textContent, name: document.querySelector('#add-name').value, count: document.querySelectorAll('#passkey-list li button').length };
  navigator.credentials.create = original;
  return result;
})()`);
if (!cancelAdd.message.includes('cancelled') || cancelAdd.name !== 'Cancelled key' || cancelAdd.count !== 2) throw new Error(`cancel flow failed: ${JSON.stringify(cancelAdd)}`);
const initialCredentialStates = (await cdp('WebAuthn.getCredentials', { authenticatorId })).credentials;
const initialCredentialId = originalCredentialState.credentials[0].credentialId;
const stateA = initialCredentialStates.find((credential) => credential.credentialId === initialCredentialId);
const stateB = initialCredentialStates.find((credential) => credential.credentialId !== initialCredentialId);
if (!stateA || !stateB) throw new Error('expected the primary and secondary virtual passkeys');

function credentialId(state) { return Buffer.from(state.credentialId, 'base64').toString('base64url'); }
function credentialIdSet(states) { return new Set(states.map((state) => state.credentialId)); }
async function currentCredentialStates() {
  return (await cdp('WebAuthn.getCredentials', { authenticatorId })).credentials;
}
async function useFreshAuthenticator() {
  await cdp('WebAuthn.removeVirtualAuthenticator', { authenticatorId });
  const added = await cdp('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  authenticatorId = added.authenticatorId;
}
async function restoreCredentials(states) {
  const currentIds = credentialIdSet(await currentCredentialStates());
  for (const state of states) {
    if (!currentIds.has(state.credentialId)) {
      await cdp('WebAuthn.addCredential', { authenticatorId, credential: state });
    }
  }
}
async function addVirtualPasskey(name) {
  const before = await currentCredentialStates();
  const authorization = await browserAssertion({ kind: 'add_passkey', name });
  const authorized = await finishAt('/api/admin/finish', {
    request_id: authorization.requestId,
    credential: authorization.credential,
  });
  if (authorized.status !== 200 || authorized.body.kind !== 'add_registration') {
    throw new Error(`add authorization failed for ${name}: ${JSON.stringify(authorized)}`);
  }
  const credentialsAfterAuthorization = await currentCredentialStates();
  await useFreshAuthenticator();
  const credential = await browserRegistration(authorized.body.options);
  const completed = await finishAt('/api/admin/add/finish', {
    request_id: authorized.body.request_id,
    credential,
  });
  if (completed.status !== 200) throw new Error(`registration failed for ${name}: ${JSON.stringify(completed)}`);
  const beforeIds = credentialIdSet(before);
  const added = (await currentCredentialStates()).find((state) => !beforeIds.has(state.credentialId));
  if (!added) throw new Error(`virtual authenticator did not create ${name}`);
  await restoreCredentials(credentialsAfterAuthorization);
  return added;
}
async function withOnlyCredential(credential, action) {
  const saved = await currentCredentialStates();
  for (const state of saved) {
    if (state.credentialId !== credential.credentialId) {
      await cdp('WebAuthn.removeCredential', {
        authenticatorId,
        credentialId: state.credentialId,
      });
    }
  }
  try {
    return await action();
  } finally {
    const currentIds = credentialIdSet(await currentCredentialStates());
    for (const state of saved) {
      if (!currentIds.has(state.credentialId)) {
        await cdp('WebAuthn.addCredential', { authenticatorId, credential: state });
      }
    }
  }
}
async function listPasskeysWith(credential) {
  const assertion = await withOnlyCredential(credential, () => browserAssertion());
  const response = await finishAt('/api/admin/finish', {
    request_id: assertion.requestId,
    credential: assertion.credential,
  });
  if (response.status !== 200) throw new Error(`passkey list failed: ${JSON.stringify(response)}`);
  return response.body.passkeys;
}
async function concurrentFinish(path, body) {
  return evaluate(`Promise.all([1, 2].map(() => fetch(${JSON.stringify(path)}, {
    method:'POST', headers:{'Content-Type':'application/json'}, body:${JSON.stringify(JSON.stringify(body))}
  }).then(response => response.status)))`);
}

const stateC = await addVirtualPasskey('QA tertiary');
const stateD = await addVirtualPasskey('QA concurrent removal target');
let passkeys = await listPasskeysWith(stateA);
if (passkeys.length !== 4) throw new Error(`expected four passkeys before mutation races, got ${passkeys.length}`);

const concurrentRemovalAssertion = await withOnlyCredential(stateB, () =>
  browserAssertion({ kind: 'remove_passkey', id: credentialId(stateD) }));
const concurrentRemovalBody = {
  request_id: concurrentRemovalAssertion.requestId,
  credential: concurrentRemovalAssertion.credential,
};
const concurrentRemovalStatuses = await concurrentFinish('/api/admin/finish', concurrentRemovalBody);
if (concurrentRemovalStatuses.slice().sort().join('/') !== '200/409') {
  throw new Error(`concurrent assertion completion results were ${concurrentRemovalStatuses}`);
}
passkeys = await listPasskeysWith(stateB);
if (passkeys.length !== 3 || passkeys.some((passkey) => passkey.id === credentialId(stateD))) {
  throw new Error(`concurrent assertion completion did not remove exactly one passkey: ${JSON.stringify(passkeys)}`);
}

const addEAuthorization = await withOnlyCredential(stateA, () =>
  browserAssertion({ kind: 'add_passkey', name: 'QA concurrent registration' }));
const addEAuthorized = await finishAt('/api/admin/finish', {
  request_id: addEAuthorization.requestId,
  credential: addEAuthorization.credential,
});
if (addEAuthorized.status !== 200 || addEAuthorized.body.kind !== 'add_registration') {
  throw new Error(`concurrent registration authorization failed: ${JSON.stringify(addEAuthorized)}`);
}
const credentialsBeforeE = await currentCredentialStates();
await useFreshAuthenticator();
const credentialE = await browserRegistration(addEAuthorized.body.options);
const concurrentRegistrationBody = {
  request_id: addEAuthorized.body.request_id,
  credential: credentialE,
};
const concurrentRegistrationStatuses = await concurrentFinish('/api/admin/add/finish', concurrentRegistrationBody);
if (concurrentRegistrationStatuses.slice().sort().join('/') !== '200/409') {
  throw new Error(`concurrent registration completion results were ${concurrentRegistrationStatuses}`);
}
await restoreCredentials(credentialsBeforeE);
const stateE = (await currentCredentialStates()).find((state) =>
  !credentialIdSet(initialCredentialStates.concat(stateC, stateD)).has(state.credentialId));
if (!stateE) throw new Error('virtual authenticator did not retain the concurrent registration');
passkeys = await listPasskeysWith(stateB);
if (passkeys.length !== 4 || !passkeys.some((passkey) => passkey.name === 'QA concurrent registration')) {
  throw new Error(`concurrent registration did not add exactly one passkey: ${JSON.stringify(passkeys)}`);
}

const expiredAssertion = await withOnlyCredential(stateA, () => browserAssertion());
const expiredAddAuthorization = await withOnlyCredential(stateB, () =>
  browserAssertion({ kind: 'add_passkey', name: 'QA expired registration' }));
const expiredAddAuthorized = await finishAt('/api/admin/finish', {
  request_id: expiredAddAuthorization.requestId,
  credential: expiredAddAuthorization.credential,
});
if (expiredAddAuthorized.status !== 200 || expiredAddAuthorized.body.kind !== 'add_registration') {
  throw new Error(`expired registration authorization failed: ${JSON.stringify(expiredAddAuthorized)}`);
}
const credentialsBeforeExpiry = await currentCredentialStates();
await useFreshAuthenticator();
const expiredRegistrationCredential = await browserRegistration(expiredAddAuthorized.body.options);
await restoreCredentials(credentialsBeforeExpiry);
await sleep(125_000);
const expiredAssertionStatus = await finishAt('/api/admin/finish', {
  request_id: expiredAssertion.requestId,
  credential: expiredAssertion.credential,
});
if (expiredAssertionStatus.status !== 409) {
  throw new Error(`valid assertion completed after expiry with status ${expiredAssertionStatus.status}`);
}
const expiredRegistrationStatus = await finishAt('/api/admin/add/finish', {
  request_id: expiredAddAuthorized.body.request_id,
  credential: expiredRegistrationCredential,
});
if (expiredRegistrationStatus.status !== 409) {
  throw new Error(`valid registration completed after expiry with status ${expiredRegistrationStatus.status}`);
}
passkeys = await listPasskeysWith(stateB);
if (passkeys.length !== 4 || passkeys.some((passkey) => passkey.name === 'QA expired registration')) {
  throw new Error(`expired ceremonies changed the passkey list: ${JSON.stringify(passkeys)}`);
}

const revokedAssertion = await withOnlyCredential(stateA, () => browserAssertion());
const removeAAssertion = await withOnlyCredential(stateB, () =>
  browserAssertion({ kind: 'remove_passkey', id: credentialId(stateA) }));
const removeAResult = await finishAt('/api/admin/finish', {
  request_id: removeAAssertion.requestId,
  credential: removeAAssertion.credential,
});
if (removeAResult.status !== 200) throw new Error(`could not remove assertion authorizer: ${JSON.stringify(removeAResult)}`);
const revokedAssertionResult = await finishAt('/api/admin/finish', {
  request_id: revokedAssertion.requestId,
  credential: revokedAssertion.credential,
});
if (revokedAssertionResult.status !== 403) {
  throw new Error(`assertion completed after its credential was removed: ${revokedAssertionResult.status}`);
}
passkeys = await listPasskeysWith(stateC);
if (passkeys.length !== 3 || passkeys.some((passkey) => passkey.id === credentialId(stateA))) {
  throw new Error(`removed assertion authorizer retained authority: ${JSON.stringify(passkeys)}`);
}

const addFAuthorization = await withOnlyCredential(stateB, () =>
  browserAssertion({ kind: 'add_passkey', name: 'QA rejected after authorizer removal' }));
const addFAuthorized = await finishAt('/api/admin/finish', {
  request_id: addFAuthorization.requestId,
  credential: addFAuthorization.credential,
});
if (addFAuthorized.status !== 200 || addFAuthorized.body.kind !== 'add_registration') {
  throw new Error(`second registration authorization failed: ${JSON.stringify(addFAuthorized)}`);
}
const credentialsBeforeRevokedRegistration = await currentCredentialStates();
await useFreshAuthenticator();
const rejectedRegistrationCredential = await browserRegistration(addFAuthorized.body.options);
await restoreCredentials(credentialsBeforeRevokedRegistration);
const removeBAssertion = await withOnlyCredential(stateC, () =>
  browserAssertion({ kind: 'remove_passkey', id: credentialId(stateB) }));
const removeBResult = await finishAt('/api/admin/finish', {
  request_id: removeBAssertion.requestId,
  credential: removeBAssertion.credential,
});
if (removeBResult.status !== 200) throw new Error(`could not remove registration authorizer: ${JSON.stringify(removeBResult)}`);
const rejectedRegistration = await finishAt('/api/admin/add/finish', {
  request_id: addFAuthorized.body.request_id,
  credential: rejectedRegistrationCredential,
});
if (rejectedRegistration.status !== 403) {
  throw new Error(`registration completed after its authorizer was removed: ${rejectedRegistration.status}`);
}
passkeys = await listPasskeysWith(stateC);
if (passkeys.length !== 2 || passkeys.some((passkey) => passkey.name === 'QA rejected after authorizer removal')) {
  throw new Error(`revoked registration authorizer added a passkey: ${JSON.stringify(passkeys)}`);
}

const refreshedList = await evaluate(`(async () => {
  document.querySelector('#list-passkeys').click();
  await new Promise(resolve => setTimeout(resolve, 900));
  return { message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length };
})()`);
if (refreshedList.count !== 2 || refreshedList.message !== 'Passkey list updated.') {
  throw new Error(`UI did not refresh after security regression checks: ${JSON.stringify(refreshedList)}`);
}
const removeUntilOne = await evaluate(`(async () => {
  let count = document.querySelectorAll('#passkey-list li button').length;
  while (count > 1) {
    document.querySelector('#passkey-list button').click();
    await new Promise(resolve => setTimeout(resolve, 900));
    count = document.querySelectorAll('#passkey-list li button').length;
    if (document.querySelector('#message').textContent !== 'Passkey removed.') throw new Error('passkey removal failed');
  }
  return { message: document.querySelector('#message').textContent, count };
})()`);
if (removeUntilOne.count !== 1) throw new Error(`could not retain a single passkey: ${JSON.stringify(removeUntilOne)}`);
const lastRemove = await evaluate(`(async () => {
  document.querySelector('#passkey-list button').click();
  await new Promise(resolve => setTimeout(resolve, 900));
  return { message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length };
})()`);
if (!lastRemove.message.includes('at least one') || lastRemove.count !== 1) throw new Error(`last-passkey guard failed: ${JSON.stringify(lastRemove)}`);
async function browserAssertion(operation = { kind: 'list_passkeys' }, mutateChallenge = false) {
  return evaluate(`(async () => {
    const start = await fetch('/api/admin/challenge', { method:'POST', headers:{'Content-Type':'application/json'}, body:${JSON.stringify(JSON.stringify(operation))} }).then(r => r.json());
    const options = structuredClone(start.options.publicKey);
    const decode = value => Uint8Array.from(atob(value.replaceAll('-','+').replaceAll('_','/').padEnd(Math.ceil(value.length/4)*4,'=')), c => c.charCodeAt(0));
    options.challenge = decode(options.challenge);
    if (${mutateChallenge}) options.challenge[0] ^= 1;
    for (const item of options.allowCredentials || []) item.id = decode(item.id);
    const assertion = await navigator.credentials.get({publicKey: options});
    return { requestId:start.request_id, credential:assertion.toJSON(), uv:options.userVerification };
  })()`);
}
async function browserRegistration(options) {
  const result = await evaluate(`(async () => {
    try {
      const publicKey = structuredClone(${JSON.stringify(options.publicKey)});
      const decode = value => Uint8Array.from(atob(value.replaceAll('-','+').replaceAll('_','/').padEnd(Math.ceil(value.length/4)*4,'=')), c => c.charCodeAt(0));
      publicKey.challenge = decode(publicKey.challenge);
      if (publicKey.user) publicKey.user.id = decode(publicKey.user.id);
      for (const item of publicKey.excludeCredentials || []) item.id = decode(item.id);
      return (await navigator.credentials.create({ publicKey })).toJSON();
    } catch (error) {
      return { qaError: String(error.name) + ': ' + String(error.message) };
    }
  })()`);
  if (result.qaError) throw new Error(`browser registration failed: ${result.qaError}`);
  return result;
}
async function finishAt(path, body) {
  return evaluate(`(async () => {
    const response = await fetch(${JSON.stringify(path)}, {method:'POST',headers:{'Content-Type':'application/json'},body:${JSON.stringify(JSON.stringify(body))}});
    let result = null;
    try { result = await response.json(); } catch {}
    return { status: response.status, body: result };
  })()`);
}
async function finish(body) {
  return (await finishAt('/api/admin/finish', body)).status;
}
function bytes(value) { return Buffer.from(value, 'base64url'); }
function b64url(value) { return Buffer.from(value).toString('base64url'); }
function signedCredential(credential, credentialState, changeAuthData) {
  if (!credentialState) throw new Error('no matching virtual authenticator credential was found');
  // Re-sign the modified fixture to isolate the verifier's origin, RP ID, and UV checks.
  const response = credential.response;
  const authData = bytes(response.authenticatorData);
  changeAuthData(authData);
  const clientData = bytes(response.clientDataJSON);
  const clientDataHash = createHash('sha256').update(clientData).digest();
  const privateKey = createPrivateKey({ key: Buffer.from(credentialState.privateKey, 'base64'), format: 'der', type: 'pkcs8' });
  const signature = sign('sha256', Buffer.concat([authData, clientDataHash]), privateKey);
  return { ...credential, response: { ...response, authenticatorData: b64url(authData), signature: b64url(signature) } };
}
const wrongChallengeAssertion = await browserAssertion({ kind: 'list_passkeys' }, true);
if (wrongChallengeAssertion.uv !== 'required') throw new Error(`authentication did not require user verification: ${wrongChallengeAssertion.uv}`);
const wrongChallenge = await finish({
  request_id: wrongChallengeAssertion.requestId,
  credential: wrongChallengeAssertion.credential,
});
if (wrongChallenge !== 403) throw new Error(`wrong challenge accepted: ${wrongChallenge}`);

const mutatedOperationAssertion = await browserAssertion();
const mutatedOperation = await finish({
  request_id: mutatedOperationAssertion.requestId,
  credential: mutatedOperationAssertion.credential,
  operation: { kind: 'remove_passkey', id: 'AQ' },
});
if (mutatedOperation !== 422) throw new Error(`mutated operation arguments were accepted: ${mutatedOperation}`);
const originalOperation = await finish({
  request_id: mutatedOperationAssertion.requestId,
  credential: mutatedOperationAssertion.credential,
});
if (originalOperation !== 200) throw new Error(`stored list operation did not complete: ${originalOperation}`);

const credentialStates = (await cdp('WebAuthn.getCredentials', { authenticatorId })).credentials;
const signedState = (credential) => credentialStates.find((candidate) =>
  Buffer.from(candidate.credentialId, 'base64').equals(bytes(credential.rawId))
);
const wrongOriginAssertion = await browserAssertion();
const originCredential = { ...wrongOriginAssertion.credential };
const originData = JSON.parse(bytes(originCredential.response.clientDataJSON).toString('utf8'));
originData.origin = 'https://wrong.example';
originCredential.response.clientDataJSON = b64url(Buffer.from(JSON.stringify(originData)));
const wrongOriginCredential = signedCredential(originCredential, signedState(originCredential), () => {});
const wrongOrigin = await finish({ request_id: wrongOriginAssertion.requestId, credential: wrongOriginCredential });
if (wrongOrigin !== 403) throw new Error(`wrong WebAuthn origin accepted: ${wrongOrigin}`);

const wrongRpAssertion = await browserAssertion();
const wrongRpCredential = signedCredential(wrongRpAssertion.credential, signedState(wrongRpAssertion.credential), (authData) => {
  createHash('sha256').update('wrong.example').digest().copy(authData, 0);
});
const wrongRp = await finish({ request_id: wrongRpAssertion.requestId, credential: wrongRpCredential });
if (wrongRp !== 403) throw new Error(`wrong RP ID accepted: ${wrongRp}`);

const missingUvAssertion = await browserAssertion();
const missingUvCredential = signedCredential(missingUvAssertion.credential, signedState(missingUvAssertion.credential), (authData) => {
  authData[32] &= ~0x04;
});
const missingUv = await finish({ request_id: missingUvAssertion.requestId, credential: missingUvCredential });
if (missingUv !== 403) throw new Error(`assertion without UV accepted: ${missingUv}`);

const replayAssertion = await browserAssertion();
const replayBody = { request_id: replayAssertion.requestId, credential: replayAssertion.credential };
const replayFirst = await finish(replayBody);
const replayAgain = await finish(replayBody);
if (replayFirst !== 200 || replayAgain !== 409) throw new Error(`assertion replay result was ${replayFirst}/${replayAgain}`);
const noCookies = await evaluate(`({ cookies: document.cookie, localStorage: localStorage.length, sessionStorage: sessionStorage.length })`);
await cdp('Emulation.setDeviceMetricsOverride', {
  width: 390, height: 844, deviceScaleFactor: 1, mobile: true,
});
const mobileLayout = await evaluate(`({ width: innerWidth, scrollWidth: document.documentElement.scrollWidth })`);
if (mobileLayout.scrollWidth > mobileLayout.width) throw new Error(`mobile layout overflowed: ${JSON.stringify(mobileLayout)}`);
await cdp('Emulation.setDeviceMetricsOverride', {
  width: 1280, height: 900, deviceScaleFactor: 1, mobile: false,
});
const desktopLayout = await evaluate(`({ width: innerWidth, scrollWidth: document.documentElement.scrollWidth })`);
if (desktopLayout.scrollWidth > desktopLayout.width) throw new Error(`desktop layout overflowed: ${JSON.stringify(desktopLayout)}`);
console.log(JSON.stringify({
  chromium: chromeVersion,
  invalidSetupCode: invalidSetup,
  setupUserVerification: setupUv,
  adminUserVerification: wrongChallengeAssertion.uv,
  setup: 'passed',
  listPasskeys: firstList.count,
  sendKeyCreatedOnce: firstSendKey.secret.startsWith('chime_sk_'),
  sendKeyDigestStored: storedSendKeyDigest.length === 1 && storedSendKeyDigest[0][0] === expectedSendKeyDigest,
  sendKeyListMetadataOnly: !firstSendKeyListed.text.includes(firstSendKey.secret),
  revokedSendKey: revokedKeySend.status,
  independentSendKey: secondKeyNoDevices.body.outcome,
  deviceEnrollmentUpsert: firstEnrollment.body.device.id === secondEnrollment.body.device.id,
  deviceTokenRotated: firstEnrollment.body.token !== secondEnrollment.body.token,
  deviceTokenDigestStored: storedDeviceTokenDigest.length === 1 && storedDeviceTokenDigest[0][0] === expectedDeviceTokenDigest,
  deviceRemoval: removedDevice.body.devices.length,
  serviceWorkerPayloadValidation: payloadValidation,
  notificationDisplay: displayedPush,
  malformedPushFallback: fallbackPush,
  addPasskey: addSecondResult.count,
  addCancellation: 'passed with input preserved',
  removal: 'passed',
  lastPasskeyRemoval: 'rejected and retained',
  wrongChallenge,
  mutatedOperationArguments: mutatedOperation,
  wrongWebAuthnOrigin: wrongOrigin,
  wrongRpId: wrongRp,
  missingUserVerification: missingUv,
  assertionReplay: replayAgain,
  expiredAssertionCompletion: expiredAssertionStatus.status,
  expiredRegistrationCompletion: expiredRegistrationStatus.status,
  revokedAssertionAuthorizer: revokedAssertionResult.status,
  revokedRegistrationAuthorizer: rejectedRegistration.status,
  concurrentAssertionMutation: concurrentRemovalStatuses,
  concurrentRegistrationMutation: concurrentRegistrationStatuses,
  noCookies,
  mobileLayout,
  desktopLayout,
}));
await cdp('WebAuthn.removeVirtualAuthenticator', { authenticatorId });
socket.close();
} finally {
  if (socket && socket.readyState < WebSocket.CLOSING) socket.close();
  await stopProcess(browser);
  await stopProcess(service, 'SIGINT');
  if (proxy?.listening) await new Promise((resolve) => proxy.close(resolve));
  fs.rmSync(workspace, { recursive: true, force: true });
}
