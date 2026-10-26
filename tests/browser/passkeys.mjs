import { createHash, createPrivateKey, sign } from 'node:crypto';
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
let service;
let browser;
let proxy;
let socket;
let authenticatorId;
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
socket.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);
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
const removeOne = await evaluate(`(async () => {
  document.querySelector('#passkey-list button').click();
  await new Promise(resolve => setTimeout(resolve, 1000));
  return { message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length };
})()`);
if (removeOne.count !== 1 || removeOne.message !== 'Passkey removed.') throw new Error(`remove failed: ${JSON.stringify(removeOne)}`);
const lastRemove = await evaluate(`(async () => {
  document.querySelector('#passkey-list button').click();
  await new Promise(resolve => setTimeout(resolve, 1000));
  return { message: document.querySelector('#message').textContent, count: document.querySelectorAll('#passkey-list li button').length };
})()`);
if (!lastRemove.message.includes('at least one') || lastRemove.count !== 1) throw new Error(`last-passkey guard failed: ${JSON.stringify(lastRemove)}`);
async function browserAssertion(mutateChallenge = false) {
  return evaluate(`(async () => {
    const start = await fetch('/api/admin/challenge', { method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({kind:'list_passkeys'}) }).then(r => r.json());
    const options = structuredClone(start.options.publicKey);
    const decode = value => Uint8Array.from(atob(value.replaceAll('-','+').replaceAll('_','/').padEnd(Math.ceil(value.length/4)*4,'=')), c => c.charCodeAt(0));
    options.challenge = decode(options.challenge);
    if (${mutateChallenge}) options.challenge[0] ^= 1;
    for (const item of options.allowCredentials || []) item.id = decode(item.id);
    const assertion = await navigator.credentials.get({publicKey: options});
    return { requestId:start.request_id, credential:assertion.toJSON(), uv:options.userVerification };
  })()`);
}
async function finish(body) {
  return evaluate(`fetch('/api/admin/finish', {method:'POST',headers:{'Content-Type':'application/json'},body:${JSON.stringify(JSON.stringify(body))}}).then(response => response.status)`);
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
const wrongChallengeAssertion = await browserAssertion(true);
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
