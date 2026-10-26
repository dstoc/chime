import { createHash, randomBytes } from 'node:crypto';
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

const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'chime-cors-'));
const dataDirectory = path.join(workspace, 'data');
const databasePath = path.join(dataDirectory, 'chime.sqlite3');
const serviceOutput = [];
const requests = [];
let service;
let browser;
let proxy;
let sender;
let socket;
let devtoolsPort;
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
  const portFile = path.join(profile, 'DevToolsActivePort');
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (fs.existsSync(portFile)) return Number(fs.readFileSync(portFile, 'utf8').split('\n')[0]);
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
  const apiPort = await freePort();
  const senderPort = await freePort();
  const apiOrigin = `https://localhost:${apiPort}`;
  const senderOrigin = `https://localhost:${senderPort}`;
  const certPath = path.join(workspace, 'cert.pem');
  const keyPath = path.join(workspace, 'key.pem');
  execFileSync('openssl', [
    'req', '-x509', '-newkey', 'rsa:2048', '-keyout', keyPath, '-out', certPath,
    '-sha256', '-days', '1', '-nodes', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost',
  ], { stdio: 'ignore' });
  const tls = { key: fs.readFileSync(keyPath), cert: fs.readFileSync(certPath) };

  proxy = https.createServer(tls, (request, response) => {
    const record = { method: request.method, path: request.url };
    requests.push(record);
    const upstream = http.request({
      host: '127.0.0.1', port: 8080, method: request.method, path: request.url,
      headers: { ...request.headers, host: '127.0.0.1:8080' },
    }, (upstreamResponse) => {
      record.responseHeaders = {
        allowOrigin: upstreamResponse.headers['access-control-allow-origin'] || null,
        allowMethods: upstreamResponse.headers['access-control-allow-methods'] || null,
        allowHeaders: upstreamResponse.headers['access-control-allow-headers'] || null,
        allowCredentials: upstreamResponse.headers['access-control-allow-credentials'] || null,
      };
      response.writeHead(upstreamResponse.statusCode, upstreamResponse.headers);
      upstreamResponse.pipe(response);
    });
    upstream.on('error', () => { response.writeHead(502); response.end(); });
    request.pipe(upstream);
  });
  await new Promise((resolve, reject) => {
    proxy.once('error', reject);
    proxy.listen(apiPort, '127.0.0.1', resolve);
  });
  sender = https.createServer(tls, (_request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
    response.end('<!doctype html><title>Cross origin sender</title>');
  });
  await new Promise((resolve, reject) => {
    sender.once('error', reject);
    sender.listen(senderPort, '127.0.0.1', resolve);
  });

  service = spawn(serverBinary, ['--origin', apiOrigin, '--data-dir', dataDirectory], {
    cwd: repoRoot,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  service.stdout.setEncoding('utf8');
  service.stderr.setEncoding('utf8');
  service.stdout.on('data', (text) => serviceOutput.push(text));
  service.stderr.on('data', (text) => serviceOutput.push(text));
  await waitForSetupCode();

  const apiKey = `chime_sk_${randomBytes(32).toString('base64url')}`;
  const digest = createHash('sha256').update(apiKey).digest();
  const script = 'import json, sqlite3, sys; value=json.loads(sys.stdin.read()); db=sqlite3.connect(sys.argv[1]); db.execute("INSERT INTO send_keys (id, name, secret_digest, created_at) VALUES (?, ?, ?, unixepoch())", (value["id"], "cross-origin test", bytes.fromhex(value["digest"]))); db.commit(); db.close()';
  execFileSync('python3', ['-c', script, databasePath], {
    input: JSON.stringify({ id: randomBytes(16).toString('base64url'), digest: digest.toString('hex') }),
    encoding: 'utf8',
  });

  const profile = path.join(workspace, 'browser');
  browser = spawn(chromiumPath, [
    '--headless=new', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--ignore-certificate-errors', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
    '--no-first-run', '--no-default-browser-check', 'about:blank',
  ], { stdio: 'ignore' });
  devtoolsPort = await waitForBrowserPort(profile);
  const chromeVersion = execFileSync(chromiumPath, ['--version'], { encoding: 'utf8' }).trim();
  const targetResponse = await fetch(`http://127.0.0.1:${devtoolsPort}/json/new?${senderOrigin}`, { method: 'PUT' });
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

  await cdp('Page.enable');
  await cdp('Runtime.enable');
  await cdp('Page.navigate', { url: senderOrigin });
  await sleep(300);
  const result = await evaluate(`(async () => {
    const target = ${JSON.stringify(`${apiOrigin}/api/notify`)};
    const allowed = await fetch(target, {
      method: 'POST', credentials: 'omit',
      headers: { Authorization: ${JSON.stringify(`Bearer ${apiKey}`)}, 'Content-Type': 'application/json' },
      body: JSON.stringify({ title: 'Cross origin send' }),
    });
    const allowedBody = await allowed.json();
    let adminGetReadable = true;
    try { await fetch(${JSON.stringify(`${apiOrigin}/api/setup/status`)}); }
    catch { adminGetReadable = false; }
    let adminPostReadable = true;
    try {
      await fetch(${JSON.stringify(`${apiOrigin}/api/admin/challenge`)}, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ kind: 'list_passkeys' }),
      });
    } catch { adminPostReadable = false; }
    const denied = await fetch(target, {
      method: 'POST', credentials: 'omit',
      headers: { Authorization: 'Bearer chime_sk_invalid', 'Content-Type': 'application/json' },
      body: JSON.stringify({ title: 'Invalid key' }),
    });
    return {
      senderOrigin: location.origin,
      allowedStatus: allowed.status,
      allowedOutcome: allowedBody.outcome,
      adminGetReadable,
      adminPostReadable,
      deniedStatus: denied.status,
    };
  })()`);
  if (result.senderOrigin !== senderOrigin
    || result.allowedStatus !== 200
    || result.allowedOutcome !== 'no_devices'
    || result.adminGetReadable
    || result.adminPostReadable
    || result.deniedStatus !== 401) {
    throw new Error(`cross-origin policy mismatch: ${JSON.stringify(result)}`);
  }
  if (!requests.some((request) => request.method === 'OPTIONS' && request.path === '/api/notify')
    || requests.filter((request) => request.method === 'POST' && request.path === '/api/notify').length !== 2
    || requests.some((request) => request.method === 'POST' && request.path === '/api/admin/challenge')
    || requests.filter((request) => request.path === '/api/notify').some((request) =>
      request.responseHeaders?.allowOrigin !== '*'
      || request.responseHeaders?.allowCredentials !== null)
    || requests.find((request) => request.method === 'OPTIONS' && request.path === '/api/notify')
      ?.responseHeaders?.allowMethods !== 'POST, OPTIONS') {
    throw new Error(`browser did not exercise the expected preflight/send paths: ${JSON.stringify(requests)}`);
  }
  console.log(JSON.stringify({
    browser: chromeVersion,
    senderOrigin,
    apiOrigin,
    result,
    requests,
    networkPermission: 'not exercised: both test origins use loopback; verify tailnet local-network permission on target Chrome devices',
  }, null, 2));
} catch (error) {
  console.error(error);
  if (serviceOutput.length) console.error(serviceOutput.join(''));
  process.exitCode = 1;
} finally {
  if (socket) socket.close();
  await stopProcess(browser);
  await stopProcess(service);
  if (proxy) await new Promise((resolve) => proxy.close(resolve));
  if (sender) await new Promise((resolve) => sender.close(resolve));
  fs.rmSync(workspace, { recursive: true, force: true });
}
