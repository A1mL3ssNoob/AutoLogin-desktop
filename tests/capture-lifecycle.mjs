#!/usr/bin/env node
/* Real Windows Tauri/WebView2 lifecycle test, using Node 24 only.
 * Run from AutoLogin-desktop: node tests/capture-lifecycle.mjs
 * Options: --binary <exe> --scenario all|ok|redirect|hang|refused|real
 * Debug fixture hooks are required except for --scenario real.
 * APPDATA, LOCALAPPDATA and WebView profiles are temporary and isolated.
 * Cleanup terminates only the child process tree created by this script.
 */
import { spawn, execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, rm, readFile, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { createServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const option = (name, fallback) => args.includes(name) ? args[args.indexOf(name) + 1] : fallback;
const binary = resolve(option('--binary', join(here, '../src-tauri/target/debug/campus_auto_login.exe')));
const selected = option('--scenario', 'all');
const scenarios = selected === 'all' ? ['ok', 'redirect', 'hang', 'refused'] : [selected];
const timeout = 15_000;
const log = (text) => process.stdout.write(`[capture-test] ${text}\n`);
const check = (value, message) => { if (!value) throw new Error(message); };

async function bounded(promise, ms, label) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms} ms`)), ms); })]); }
  finally { clearTimeout(timer); }
}
async function until(fn, label, ms = timeout) {
  const start = Date.now(); let last;
  while (Date.now() - start < ms) {
    try { const value = await fn(); if (value) return value; } catch (error) { last = error; }
    await delay(150);
  }
  throw new Error(`${label} timed out after ${ms} ms${last ? `: ${last.message}` : ''}`);
}
async function port() {
  const server = createTcpServer();
  await new Promise((ok, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', ok); });
  const number = server.address().port;
  await new Promise((ok) => server.close(ok));
  return number;
}
function native(pid, operation = 'list', hwnd) {
  const parameters = ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', join(here, 'user32-window-helper.ps1'), '-ProcessId', String(pid), '-Operation', operation];
  if (hwnd !== undefined) parameters.push('-Hwnd', String(hwnd));
  let text;
  // PowerShell startup/Add-Type can be slow under antivirus or a concurrent build.
  // This allowance does not change the native WM_NULL response deadline of 1 s.
  try { text = execFileSync('powershell.exe', parameters, { encoding: 'utf8', timeout: 12000, windowsHide: true }); }
  catch (error) { if (!error.stdout) throw error; text = String(error.stdout); }
  const value = text.trim() ? JSON.parse(text.trim()) : null;
  return operation === 'list' ? (Array.isArray(value) ? value : value ? [value] : []) : value;
}
const handles = (pid) => new Set(native(pid).map((item) => String(item.hwnd)));
const newWindows = (pid, baseline) => native(pid).filter((item) => !baseline.has(String(item.hwnd)) && item.title);
const captureWindow = (pid, baseline) => until(() => newWindows(pid, baseline)[0], 'capture HWND creation');
const gone = (pid, hwnd) => until(() => !native(pid, 'exists', hwnd).exists, `HWND ${hwnd} destruction`, 8000);
function responsive(pid, hwnd) { check(native(pid, 'null', hwnd)?.ok, `HWND ${hwnd} did not respond to WM_NULL within 1 second`); }
async function captureInactive(root) {
  await until(async () => {
    const text = await readFile(join(root, 'local', 'CampusAutoLogin', 'logs', 'current.log'), 'utf8');
    const last = text.trim().split(/\r?\n/).map((line) => JSON.parse(line)).filter((entry) => entry.event === 'capture_changed').at(-1);
    return last?.detail === 'active=false';
  }, 'capture_changed active=false log');
}

async function targets(debugPort) {
  const response = await fetch(`http://127.0.0.1:${debugPort}/json/list`, { signal: AbortSignal.timeout(1000) });
  return response.json();
}
async function connect(debugPort, child) {
  const target = await until(async () => {
    check(child.exitCode === null, `application exited with ${child.exitCode}`);
    const list = await targets(debugPort);
    return list.find((item) => item.type === 'page' && item.webSocketDebuggerUrl && /tauri\.localhost|index\.html/i.test(item.url || '')) || list.find((item) => item.type === 'page' && item.webSocketDebuggerUrl);
  }, 'main CDP endpoint');
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await bounded(new Promise((ok, fail) => { socket.addEventListener('open', ok, { once: true }); socket.addEventListener('error', fail, { once: true }); }), 3000, 'CDP WebSocket');
  let sequence = 0;
  const pending = new Map();
  socket.addEventListener('message', ({ data }) => {
    const result = JSON.parse(data), item = pending.get(result.id);
    if (!item) return;
    pending.delete(result.id); clearTimeout(item.timer);
    result.error ? item.fail(new Error(JSON.stringify(result.error))) : item.ok(result.result);
  });
  socket.addEventListener('close', () => { for (const item of pending.values()) { clearTimeout(item.timer); item.fail(new Error('CDP target closed')); } pending.clear(); });
  const call = (method, params, ms = timeout) => new Promise((ok, fail) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); fail(new Error(`${method} timed out after ${ms} ms`)); }, ms);
    pending.set(id, { ok, fail, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression, ms = timeout) => {
    const result = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, ms);
    check(!result.exceptionDetails, `JavaScript exception: ${result.exceptionDetails?.exception?.description || result.exceptionDetails?.text}`);
    return result.result?.value;
  };
  await until(() => evaluate(`typeof window.__TAURI__?.core?.invoke === 'function' && document.readyState === 'complete'`, 2000), 'main page Tauri initialization');
  return { evaluate, close: () => socket.close() };
}
async function fixture(scenario) {
  if (scenario === 'real') return { url: undefined, requests: [], close: async () => {} };
  if (scenario === 'refused') return { url: `http://127.0.0.1:${await port()}/refused`, requests: [], close: async () => {} };
  const requests = [], sockets = new Set();
  const server = createServer((request, response) => {
    requests.push(request.url);
    if (request.url === '/hang') return;
    if (request.url === '/redirect') { response.writeHead(302, { location: '/ok' }); response.end(); return; }
    response.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
    response.end('<!doctype html><title>Capture fixture</title><h1>Local capture lifecycle fixture</h1>');
  });
  server.on('connection', (socket) => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
  await new Promise((ok, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', ok); });
  return { url: `http://127.0.0.1:${server.address().port}/${scenario}`, requests, close: async () => { for (const socket of sockets) socket.destroy(); await new Promise((ok) => server.close(ok)); } };
}
async function diagnostics(root, child) {
  if (!child?.pid) return;
  try {
    const windows = native(child.pid); log(`native failure snapshot: ${JSON.stringify(windows)}`);
    for (const window of windows.filter((item) => item.title)) {
      log(`HWND ${window.hwnd} WM_NULL: ${JSON.stringify(native(child.pid, 'null', window.hwnd))}`);
      if (/认证信息/.test(window.title)) {
        log(`HWND ${window.hwnd} WM_CLOSE: ${JSON.stringify(native(child.pid, 'close', window.hwnd))}`);
        await delay(500); log(`HWND ${window.hwnd} still exists: ${handles(child.pid).has(String(window.hwnd))}`);
      }
    }
  } catch (error) { log(`native diagnostic error: ${error.message}`); }
  const logs = join(root, 'local', 'CampusAutoLogin', 'logs');
  try { for (const name of await readdir(logs)) { const content = await readFile(join(logs, name), 'utf8'); log(`isolated log ${name}:\n${content.slice(-12000)}`); } } catch {}
}
async function runScenario(scenario) {
  const root = await mkdtemp(join(tmpdir(), 'campus-capture-test-'));
  const source = await fixture(scenario), debugPort = await port(), captureDebugPort = await port();
  const browserArgs = `--remote-debugging-port=${debugPort} --remote-debugging-address=127.0.0.1 --no-proxy-server`;
  const env = { ...process.env, APPDATA: join(root, 'roaming'), LOCALAPPDATA: join(root, 'local'),
    CAMPUS_CAPTURE_TEST_BROWSER_ARGS: browserArgs,
    CAMPUS_CAPTURE_TEST_CAPTURE_BROWSER_ARGS: `--remote-debugging-port=${captureDebugPort} --remote-debugging-address=127.0.0.1 --no-proxy-server` };
  // Keep the application's separate main/capture profiles. A global WebView2
  // directory override would collapse them into one shared browser session.
  delete env.WEBVIEW2_USER_DATA_FOLDER;
  delete env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS;
  if (source.url) env.CAMPUS_CAPTURE_TEST_URL = source.url; else delete env.CAMPUS_CAPTURE_TEST_URL;
  await Promise.all([env.APPDATA, env.LOCALAPPDATA].map((path) => mkdir(path, { recursive: true })));
  let child, client;
  try {
    log(`scenario=${scenario}; binary=${binary}; isolated data=${root}`);
    child = spawn(binary, [], { env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let spawnError;
    child.on('error', (error) => { spawnError = error; });
    child.stdout.on('data', (chunk) => process.stdout.write(chunk)); child.stderr.on('data', (chunk) => process.stderr.write(chunk));
    await delay(50); if (spawnError) throw spawnError;
    client = await connect(debugPort, child);
    const invoke = (command) => client.evaluate(`window.__TAURI__.core.invoke(${JSON.stringify(command)})`);
    const initial = native(child.pid), baseline = handles(child.pid);
    const started = Date.now(); await invoke('open_capture_window'); log(`open_capture_window returned in ${Date.now() - started} ms`);
    const first = await captureWindow(child.pid, baseline); responsive(child.pid, first.hwnd); log('capture responds to WM_NULL');
    if (['ok', 'redirect', 'hang'].includes(scenario)) await until(() => source.requests.includes(`/${scenario}`), 'fixture request (debug hook must be enabled)');
    await client.evaluate(`Promise.all([window.__TAURI__.core.invoke('open_capture_window'), window.__TAURI__.core.invoke('open_capture_window')])`);
    check(newWindows(child.pid, baseline).length === 1, 'duplicate open produced multiple capture windows'); log('duplicate open keeps one capture window');
    check(native(child.pid, 'close', first.hwnd)?.ok, 'could not post WM_CLOSE'); await gone(child.pid, first.hwnd); await captureInactive(root); log('native WM_CLOSE destroys capture window and logs active=false');
    await invoke('get_status');
    const reopenBaseline = handles(child.pid); await invoke('open_capture_window'); const reopened = await captureWindow(child.pid, reopenBaseline);
    await client.evaluate(`(async () => { await window.__TAURI__.core.invoke('close_capture_window'); await window.__TAURI__.core.invoke('open_capture_window'); return true; })()`);
    const immediatelyReopened = await captureWindow(child.pid, reopenBaseline); responsive(child.pid, immediatelyReopened.hwnd); log('back-to-back close/open leaves a responsive capture window');
    await invoke('close_capture_window'); await gone(child.pid, immediatelyReopened.hwnd); await captureInactive(root); log('reopen and main-page close command work');
    const uiBaseline = handles(child.pid);
    await client.evaluate(`(() => { document.querySelector('#first-run-modal').hidden = true; document.querySelector('[data-action="capture"]').click(); document.querySelector('#start-capture-button').click(); return true; })()`);
    const uiWindow = await captureWindow(child.pid, uiBaseline);
    await until(() => client.evaluate(`document.querySelector('#capture-progress-text').textContent !== '正在打开校园认证页面…'`, 2000), 'capture UI open completion');
    await client.evaluate(`document.querySelector('#capture-modal [data-action="close-capture"]').click(); true`);
    await gone(child.pid, uiWindow.hwnd); await captureInactive(root); log('actual main-page Cancel button destroys capture window');
    const uiRaceBaseline = handles(child.pid);
    await client.evaluate(`document.querySelector('[data-action="capture"]').click(); document.querySelector('#start-capture-button').click(); true`);
    await captureWindow(child.pid, uiRaceBaseline);
    await client.evaluate(`document.querySelector('#capture-modal [data-action="close-capture"]').click(); document.querySelector('[data-action="capture"]').click(); document.querySelector('#start-capture-button').click(); true`);
    await until(() => client.evaluate(`!document.querySelector('#capture-modal').hidden && !document.querySelector('#capture-progress').hidden && document.querySelector('#capture-progress-text').textContent.startsWith('请在认证窗口')`, 2000), 'UI Cancel then immediate restart');
    const uiRaceWindow = await captureWindow(child.pid, uiRaceBaseline); responsive(child.pid, uiRaceWindow.hwnd);
    check(newWindows(child.pid, uiRaceBaseline).length === 1, 'UI Cancel/restart produced multiple capture windows');
    await client.evaluate(`document.querySelector('#capture-modal [data-action="close-capture"]').click(); true`);
    await gone(child.pid, uiRaceWindow.hwnd); await captureInactive(root); log('UI Cancel then immediate restart produces one working window');
    const cancelBaseline = handles(child.pid);
    await client.evaluate(`window.__lifecycleOpen = window.__TAURI__.core.invoke('open_capture_window'); window.__lifecycleOpen.catch(() => {}); true`);
    await delay(30); await invoke('close_capture_window'); await client.evaluate('window.__lifecycleOpen'); await delay(500);
    check(newWindows(child.pid, cancelBaseline).length === 0, 'immediate cancel left a capture window behind'); await captureInactive(root); log('cancel during opening leaves no capture window');
    const main = initial.find((item) => item.title.includes('校园网自动登录'));
    check(main, 'could not locate main native window');
    native(child.pid, 'close', main.hwnd); await gone(child.pid, main.hwnd); await delay(500);
    check(child.exitCode === null, 'closing the final window exited the tray application'); process.kill(child.pid, 0); log('closing last window keeps tray process alive');
    log(`PASS scenario=${scenario}`);
  } catch (error) { await diagnostics(root, child); throw error; }
  finally {
    try { client?.close(); } catch {}
    if (child?.pid && child.exitCode === null) { try { execFileSync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { timeout: 8000, windowsHide: true, stdio: 'ignore' }); } catch {} }
    await source.close();
    try {
      check(dirname(resolve(root)) === resolve(tmpdir()) && resolve(root).split(/[\\/]/).at(-1).startsWith('campus-capture-test-'), 'refusing cleanup outside the generated test directory');
      await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 250 });
    }
    catch (error) { log(`temporary profile retained at ${root}: ${error.code}`); }
  }
}
async function main() {
  check(process.platform === 'win32', 'this integration test requires Windows');
  check(Number(process.versions.node.split('.')[0]) >= 24, 'Node 24 or newer required');
  check(scenarios.every((scenario) => ['ok', 'redirect', 'hang', 'refused', 'real'].includes(scenario)), 'unknown --scenario (choose all/ok/redirect/hang/refused/real)');
  for (const scenario of scenarios) await runScenario(scenario);
}
main().catch((error) => { console.error(`[capture-test] FAIL: ${error.stack || error}`); process.exitCode = 1; });
