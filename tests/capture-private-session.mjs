#!/usr/bin/env node
/* Verify that the main and capture WebView2 sessions are private.
 * Run from the repository root:
 *   node tests/capture-private-session.mjs [--binary <exe>]
 * Add --shared-profile only when reproducing the old shared-profile behavior;
 * this forces the legacy WebView2 data directory into the temporary profile.
 * A local fixture sends stale main-profile cookies to a refused endpoint.
 * The check also requires capture cookies/localStorage to disappear on close
 * and main storage to disappear on restart. No real account data is read.
 */
import { spawn, execFileSync } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const sharedProfile = args.includes('--shared-profile');
const checkedArgs = args.filter(arg => arg !== '--shared-profile');
const binary = resolve(args.includes('--binary')
  ? args[args.indexOf('--binary') + 1]
  : join(here, '../src-tauri/target/debug/campus_auto_login.exe'));
const timeout = 15_000;
const log = message => process.stdout.write(`[capture-private] ${message}\n`);
const check = (value, message) => { if (!value) throw new Error(message); };

async function until(action, label, limit = timeout) {
  const start = Date.now();
  let last;
  while (Date.now() - start < limit) {
    try {
      const value = await action();
      if (value) return value;
    } catch (error) { last = error; }
    await delay(150);
  }
  throw new Error(`${label} timed out after ${limit} ms${last ? `: ${last.message}` : ''}`);
}

async function freePort() {
  const server = createServer();
  await new Promise((ok, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', ok); });
  const value = server.address().port;
  await new Promise(ok => server.close(ok));
  return value;
}

async function fixture() {
  const refusedPort = await freePort();
  const requests = [], sockets = new Set();
  const server = createServer((request, response) => {
    if (request.url !== '/capture') {
      response.writeHead(404); response.end(); return;
    }
    const cookie = request.headers.cookie || '';
    requests.push({ cookie });
    if (cookie.includes('capture_existing_session=')) {
      response.writeHead(302, { location: `http://127.0.0.1:${refusedPort}/unreachable`, 'cache-control': 'no-store' });
      response.end(); return;
    }
    response.writeHead(200, {
      'content-type': 'text/html; charset=utf-8',
      'cache-control': 'no-store',
    });
    response.end('<!doctype html><title>Capture private-session fixture</title><h1>fixture</h1>');
  });
  server.on('connection', socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
  await new Promise((ok, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', ok); });
  return { requests, url: `http://127.0.0.1:${server.address().port}/capture`,
    close: async () => { for (const socket of sockets) socket.destroy(); await new Promise(ok => server.close(ok)); } };
}

async function targets(port) {
  const response = await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(1000) });
  return response.json();
}

async function connect(port, child, selector) {
  const target = await until(async () => {
    check(child.exitCode === null, `application exited with ${child.exitCode}`);
    const pages = await targets(port);
    return pages.find(page => page.type === 'page' && page.webSocketDebuggerUrl && selector(page));
  }, 'WebView2 CDP target');
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await until(() => socket.readyState === WebSocket.OPEN, 'CDP WebSocket', 5_000);
  let sequence = 0;
  const pending = new Map();
  socket.addEventListener('message', ({ data }) => {
    const reply = JSON.parse(data);
    const current = pending.get(reply.id);
    if (!current) return;
    pending.delete(reply.id); clearTimeout(current.timer);
    reply.error ? current.fail(new Error(JSON.stringify(reply.error))) : current.ok(reply.result);
  });
  socket.addEventListener('close', () => {
    for (const current of pending.values()) { clearTimeout(current.timer); current.fail(new Error('CDP target closed')); }
    pending.clear();
  });
  const call = (method, params = {}) => new Promise((ok, fail) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); fail(new Error(`${method} timed out`)); }, timeout);
    pending.set(id, { ok, fail, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async expression => {
    const result = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    check(!result.exceptionDetails, result.exceptionDetails?.exception?.description || result.exceptionDetails?.text || 'JavaScript exception');
    return result.result?.value;
  };
  await until(() => evaluate(`document.readyState === 'complete'`), 'page ready');
  return { call, evaluate, close: () => socket.close(), target };
}

function stop(child) {
  if (child?.pid && child.exitCode === null) {
    try { execFileSync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { timeout: 8_000, windowsHide: true, stdio: 'ignore' }); } catch {}
  }
}

async function launch(profile, fixtureUrl, debugPort, capturePort) {
  const browserArgs = `--remote-debugging-port=${debugPort} --remote-debugging-address=127.0.0.1 --no-proxy-server`;
  const env = {
    ...process.env,
    APPDATA: join(profile, 'roaming'),
    LOCALAPPDATA: join(profile, 'local'),
    CAMPUS_CAPTURE_TEST_BROWSER_ARGS: browserArgs,
    CAMPUS_CAPTURE_TEST_CAPTURE_BROWSER_ARGS: `--remote-debugging-port=${capturePort} --remote-debugging-address=127.0.0.1 --no-proxy-server`,
    CAMPUS_CAPTURE_TEST_URL: fixtureUrl,
  };
  // The app assigns independent data directories; WebView2 environment
  // overrides would collapse them back into one shared browser profile.
  delete env.WEBVIEW2_USER_DATA_FOLDER;
  delete env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS;
  if (sharedProfile) env.WEBVIEW2_USER_DATA_FOLDER = join(profile, 'webview');
  await Promise.all([env.APPDATA, env.LOCALAPPDATA].map(path => mkdir(path, { recursive: true })));
  const child = spawn(binary, [], { env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.on('data', chunk => process.stdout.write(chunk));
  child.stderr.on('data', chunk => process.stderr.write(chunk));
  return child;
}

async function main() {
  check(process.platform === 'win32', 'this integration test requires Windows');
  check(Number(process.versions.node.split('.')[0]) >= 24, 'Node 24 or newer required');
  check(checkedArgs.length === 0 || checkedArgs.length === 2 && checkedArgs[0] === '--binary' && checkedArgs[1], 'usage: capture-private-session.mjs [--binary <exe>] [--shared-profile]');
  const root = await mkdtemp(join(tmpdir(), 'campus-private-session-'));
  const source = await fixture();
  let child;
  let mainClient;
  let captureClient;
  try {
    const firstPort = await freePort();
    const capturePort = await freePort();
    child = await launch(root, source.url, firstPort, capturePort);
    mainClient = await connect(firstPort, child, page => /tauri\.localhost|index\.html/i.test(page.url || ''));
    await until(() => mainClient.evaluate(`typeof window.__TAURI__?.core?.invoke === 'function'`), 'main Tauri initialization');
    const invoke = command => mainClient.evaluate(`window.__TAURI__.core.invoke(${JSON.stringify(command)})`);
    await mainClient.call('Network.setCookie', { name: 'capture_existing_session', value: 'stale', url: source.url, path: '/', expires: Date.now() / 1000 + 3600 });
    const seeded = await mainClient.call('Network.getCookies', { urls: [source.url] });
    check(seeded.cookies.some(cookie => cookie.name === 'capture_existing_session'), 'main-profile stale cookie was not seeded');
    log('main profile contains a stale cookie that redirects the fixture to a refused connection');
    for (const attempt of [1, 2]) {
      const previousRequests = source.requests.length;
      await invoke('open_capture_window');
      const request = await until(() => source.requests[previousRequests], `capture ${attempt} HTTP fixture request`);
      check(!request.cookie.includes('capture_existing_session='), `capture ${attempt} reused the main profile stale cookie and was redirected to an unreachable page`);
      check(!request.cookie.includes('capture_private='), `capture ${attempt} reused the previous capture cookie`);
      captureClient = await connect(capturePort, child, page => page.url === source.url);
      check(await captureClient.evaluate(`!document.cookie.includes('capture_existing_session=') && !document.cookie.includes('capture_private=') && localStorage.getItem('capture_private') === null`), `capture ${attempt} inherited old cookies or localStorage`);
      await captureClient.evaluate(`document.cookie = 'capture_private=1; Path=/'; localStorage.setItem('capture_private', '1');`);
      check(await captureClient.evaluate(`document.cookie.includes('capture_private=1') && localStorage.getItem('capture_private') === '1'`), `capture ${attempt} did not retain in-session storage`);
      const targetId = captureClient.target.id;
      captureClient.close(); captureClient = undefined;
      await invoke('close_capture_window');
      await until(async () => {
        try { return !(await targets(capturePort)).some(page => page.id === targetId); }
        catch { return true; } // The isolated browser may exit with its last view.
      }, `capture ${attempt} target destruction`);
      log(`capture ${attempt} loads HTTP without inherited state and closes normally`);
    }
    const retained = await mainClient.call('Network.getCookies', { urls: [source.url] });
    check(retained.cookies.some(cookie => cookie.name === 'capture_existing_session'), 'capture cleanup unexpectedly removed main-profile storage');
    check(!retained.cookies.some(cookie => cookie.name === 'capture_private'), 'capture cookies leaked into main profile');

    const marker = `private-${Date.now()}`;
    await mainClient.evaluate(`document.cookie = ${JSON.stringify(`main_private=${marker}; Path=/`)}; localStorage.setItem('main_private', ${JSON.stringify(marker)});`);
    check(await mainClient.evaluate(`document.cookie.includes('main_private=') && localStorage.getItem('main_private') !== null`), 'main WebView did not retain in-session storage');
    stop(child); child = undefined; mainClient.close(); mainClient = undefined;
    await delay(300);

    const secondPort = await freePort();
    child = await launch(root, source.url, secondPort, await freePort());
    mainClient = await connect(secondPort, child, page => /tauri\.localhost|index\.html/i.test(page.url || ''));
    check(await mainClient.evaluate(`!document.cookie.includes('main_private=') && localStorage.getItem('main_private') === null`), 'main WebView persisted private storage across restart');
    const restartedCookies = await mainClient.call('Network.getCookies', { urls: [source.url] });
    check(!restartedCookies.cookies.some(cookie => cookie.name === 'capture_existing_session'), 'main-profile test cookie survived application restart');
    log('PASS: main and capture sessions are private; capture HTTP is isolated from stale profile data');
  } finally {
    // Remove only this test's synthetic cookie even when an assertion fails.
    try { await mainClient?.call('Network.deleteCookies', { name: 'capture_existing_session', url: source.url }); } catch {}
    try { captureClient?.close(); } catch {}
    try { mainClient?.close(); } catch {}
    stop(child);
    await source.close();
    try {
      check(dirname(resolve(root)) === resolve(tmpdir()) && resolve(root).split(/[\\/]/).at(-1).startsWith('campus-private-session-'), 'refusing unsafe temporary profile cleanup');
      await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 250 });
    } catch (error) { log(`temporary profile retained at ${root} (${error.code || error.message})`); }
  }
}

main().catch(error => { console.error(`[capture-private] FAIL: ${error.stack || error}`); process.exitCode = 1; });
