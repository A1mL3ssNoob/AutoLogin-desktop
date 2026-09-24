#!/usr/bin/env node
/* Real WebView2 layout regression for the Windows main window.
 * Run: node tests/main-layout.mjs [--binary <exe>]
 * The executable uses a fresh temporary app/profile directory. Only its own
 * process tree and directory are removed after the check.
 */
import { spawn, execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, rm, writeFile, appendFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { createServer } from 'node:net';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const binary = resolve(args.includes('--binary') ? args[args.indexOf('--binary') + 1] : join(here, '../src-tauri/target/debug/campus_auto_login.exe'));
const log = message => process.stdout.write(`[main-layout] ${message}\n`);
const check = (condition, message) => { if (!condition) throw new Error(message); };

async function until(action, label, limit = 20000) {
  const start = Date.now(); let last;
  while (Date.now() - start < limit) {
    try { const result = await action(); if (result) return result; } catch (error) { last = error; }
    await delay(150);
  }
  throw new Error(`${label} timed out after ${limit} ms${last ? `: ${last.message}` : ''}`);
}

async function freePort() {
  const server = createServer();
  await new Promise((ok, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', ok); });
  const number = server.address().port;
  await new Promise(ok => server.close(ok));
  return number;
}

async function connect(port, child) {
  const target = await until(async () => {
    check(child.exitCode === null, `app exited with code ${child.exitCode}`);
    const response = await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(1000) });
    const pages = await response.json();
    return pages.find(page => page.type === 'page' && page.webSocketDebuggerUrl && /tauri\.localhost|index\.html/i.test(page.url || ''));
  }, 'main WebView2 debug endpoint');
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await Promise.race([
    new Promise((ok, fail) => { socket.addEventListener('open', ok, { once: true }); socket.addEventListener('error', fail, { once: true }); }),
    delay(5000).then(() => { throw new Error('WebView2 debug socket timed out'); }),
  ]);
  let sequence = 0;
  const pending = new Map();
  socket.addEventListener('message', ({ data }) => {
    const reply = JSON.parse(data); const current = pending.get(reply.id);
    if (!current) return;
    pending.delete(reply.id); clearTimeout(current.timer);
    reply.error ? current.fail(new Error(JSON.stringify(reply.error))) : current.ok(reply.result);
  });
  socket.addEventListener('close', () => {
    for (const current of pending.values()) { clearTimeout(current.timer); current.fail(new Error('WebView2 debug socket closed')); }
    pending.clear();
  });
  async function evaluate(expression, limit = 5000) {
    const reply = await new Promise((ok, fail) => {
      const id = ++sequence;
      const timer = setTimeout(() => { pending.delete(id); fail(new Error(`Runtime.evaluate timed out after ${limit} ms`)); }, limit);
      pending.set(id, { ok, fail, timer });
      socket.send(JSON.stringify({ id, method: 'Runtime.evaluate', params: { expression, awaitPromise: true, returnByValue: true } }));
    });
    check(!reply.exceptionDetails, reply.exceptionDetails?.exception?.description || reply.exceptionDetails?.text || 'JavaScript exception');
    return reply.result?.value;
  }
  await until(() => evaluate(`document.readyState === 'complete' && !!document.querySelector('#view-overview')`), 'main page load');
  return { evaluate, close: () => socket.close() };
}

const measurement = `(() => {
  const names = {
    setup: ['#first-run-modal .first-run-modal', '#welcome-phone', '#welcome-uid', '#first-run-form button[type="submit"]'],
    overview: ['#view-overview .hero-card', '#view-overview .metrics-grid', '#view-overview .content-grid', '#view-overview [data-action="capture"]'],
    account: ['#view-account .config-panel', '#view-account #account-form button[type="submit"]', '#view-account #service-name', '#view-account #apartment-id', '#view-account #room-id', '#view-account .capture-panel', '#view-account .startup-panel'],
    logs: ['#view-logs .logs-intro', '#view-logs [data-action="export-logs"]', '#view-logs .log-toolbar', '#view-logs .log-panel'],
  };
  const view = window.__layoutView;
  const viewport = { width: innerWidth, height: innerHeight, documentClientWidth: document.documentElement.clientWidth,
    bodyScrollWidth: document.body.scrollWidth, documentScrollWidth: document.documentElement.scrollWidth,
    bodyScrollHeight: document.body.scrollHeight, documentScrollHeight: document.documentElement.scrollHeight };
  const elements = names[view].map(selector => {
    const element = document.querySelector(selector);
    if (!element) return { selector, missing: true };
    const r = element.getBoundingClientRect();
    return { selector, visible: getComputedStyle(element).display !== 'none' && r.width > 0 && r.height > 0,
      editable: !element.readOnly && !element.disabled,
      left: Math.round(r.left), top: Math.round(r.top), right: Math.round(r.right), bottom: Math.round(r.bottom),
      width: Math.round(r.width), height: Math.round(r.height) };
  });
  return { view, viewport, elements };
})()`;

const logSnapshot = `(() => {
  const list = document.querySelector('#log-list');
  const rows = [...list.querySelectorAll('.log-row')];
  const last = rows.at(-1);
  const bounds = list.getBoundingClientRect();
  const lastBounds = last?.getBoundingClientRect();
  return {
    count: rows.length,
    hiddenProbeShown: rows.some(row => row.querySelector('.log-kind')?.textContent === '网络正常' || row.textContent.includes('layout-seed-probe-')),
    lastText: last?.textContent || '',
    newEventShown: rows.some(row => row.textContent.includes('layout-background-refresh-event')),
    overflow: list.scrollHeight > list.clientHeight,
    bottomGap: list.scrollHeight - list.clientHeight - list.scrollTop,
    lastVisible: !!lastBounds && lastBounds.top >= bounds.top - 1 && lastBounds.bottom <= bounds.bottom + 1,
  };
})()`;

async function run() {
  check(process.platform === 'win32', 'requires Windows');
  check(Number(process.versions.node.split('.')[0]) >= 24, 'requires Node 24 or newer');
  check(args.length === 0 || args.length === 2 && args[0] === '--binary' && args[1], 'usage: main-layout.mjs [--binary <exe>]');
  const root = await mkdtemp(join(tmpdir(), 'campus-main-layout-'));
  const profile = { roaming: join(root, 'roaming'), local: join(root, 'local'), webview: join(root, 'webview') };
  await Promise.all(Object.values(profile).map(path => mkdir(path, { recursive: true })));
  const logDir = join(profile.local, 'CampusAutoLogin', 'logs');
  const logFile = join(logDir, 'current.log');
  const seedRecords = Array.from({ length: 45 }, (_, index) => ({
    ts: Math.floor(Date.now() / 1000) - 60 + index,
    level: 'INFO',
    event: index % 5 === 0 ? 'probe_online' : 'auth_attempt',
    detail: index % 5 === 0 ? `layout-seed-probe-${index}` : `layout-seed-useful-${index}`,
  }));
  await mkdir(logDir, { recursive: true });
  await writeFile(logFile, `${seedRecords.map(record => JSON.stringify(record)).join('\n')}\n`);
  const usefulSeedCount = seedRecords.filter(record => record.event !== 'probe_online').length;
  const port = await freePort();
  const browserArgs = `--remote-debugging-port=${port} --remote-debugging-address=127.0.0.1 --no-proxy-server`;
  const env = { ...process.env, APPDATA: profile.roaming, LOCALAPPDATA: profile.local,
    WEBVIEW2_USER_DATA_FOLDER: profile.webview, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: browserArgs,
    CAMPUS_CAPTURE_TEST_BROWSER_ARGS: browserArgs };
  let child, client;
  try {
    log(`binary=${binary}`);
    child = spawn(binary, [], { env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    child.stdout.on('data', chunk => process.stdout.write(chunk));
    child.stderr.on('data', chunk => process.stderr.write(chunk));
    let spawnError;
    child.on('error', error => { spawnError = error; });
    await delay(50); if (spawnError) throw spawnError;
    client = await connect(port, child);
    await until(() => client.evaluate(`!document.querySelector('#first-run-modal').hidden`), 'first-run dialog');
    const results = [];
    for (const view of ['setup', 'overview', 'account', 'logs']) {
      if (view !== 'setup') {
        await client.evaluate(`document.querySelector('#first-run-modal').hidden = true; document.querySelector('[data-view="${view}"]').click(); window.__layoutView = '${view}'; true`);
        await delay(350); // The CSS view transition lasts 250 ms.
      } else await client.evaluate(`window.__layoutView = 'setup'; true`);
      const result = await client.evaluate(measurement);
      results.push(result);
      log(JSON.stringify(result));
      if (view === 'overview') {
        const activity = await client.evaluate(`(() => {
          const rows = [...document.querySelectorAll('#activity-list .log-row')];
          return { count: rows.length, hiddenProbeShown: rows.some(row => row.querySelector('.log-kind')?.textContent === '网络正常' || row.textContent.includes('layout-seed-probe-')) };
        })()`);
        check(activity.count === 3 && !activity.hiddenProbeShown, `recent activity contains routine probe logs: ${JSON.stringify(activity)}`);
        log('recent activity omits routine probe_online records');
      }
      if (view === 'account') {
        const carrier = await client.evaluate(`(() => {
          const select = document.querySelector('#service-name');
          const values = [...select.options].map(option => option.value);
          const initial = select.value;
          select.value = 'chinaMobile';
          return { values, initial, selected: select.value, editable: !select.disabled };
        })()`);
        check(JSON.stringify(carrier.values) === JSON.stringify(['chinaTelecom', 'chinaUnicom', 'chinaMobile']) && carrier.initial === 'chinaTelecom' && carrier.selected === 'chinaMobile' && carrier.editable,
          `carrier selector is missing or not editable: ${JSON.stringify(carrier)}`);
        log('carrier selector exposes the three campus service options');
      }
      if (view === 'logs') {
        const initialLogs = await until(async () => {
          const state = await client.evaluate(logSnapshot);
          return state.count >= usefulSeedCount && state;
        }, 'seeded logs in running log view');
        check(!initialLogs.hiddenProbeShown, 'running log view displays routine probe_online records');
        check(initialLogs.overflow && initialLogs.bottomGap <= 2 && initialLogs.lastVisible,
          `running log view did not open at the latest record: ${JSON.stringify(initialLogs)}`);
        log(`running log view omits probe_online and opens at latest of ${initialLogs.count} records`);
        const logLayout = await client.evaluate(`(() => {
          const list = document.querySelector('#log-list');
          list.innerHTML = '<div class="log-row"><span class="log-time">12:00:00</span><span class="log-kind">capture_window_create_failed_due_to_webview_initialization</span><span class="log-message">窗口创建失败的详细诊断信息</span></div>';
          const row = list.querySelector('.log-row');
          const badge = list.querySelector('.log-kind');
          const rowRect = row.getBoundingClientRect();
          const badgeRect = badge.getBoundingClientRect();
          return { rowWidth: row.clientWidth, rowScrollWidth: row.scrollWidth, badgeRight: badgeRect.right, rowRight: rowRect.right, overflow: getComputedStyle(badge).overflow, textOverflow: getComputedStyle(badge).textOverflow };
        })()`);
        check(logLayout.rowScrollWidth <= logLayout.rowWidth + 1 && logLayout.badgeRight <= logLayout.rowRight + 1 && logLayout.overflow === 'hidden' && logLayout.textOverflow === 'ellipsis', 'long log event text escapes its colored label');
        log('long log event text stays inside its colored label');
        await appendFile(logFile, `${JSON.stringify({ ts: Math.floor(Date.now() / 1000), level: 'INFO', event: 'auth_success', detail: 'layout-background-refresh-event' })}\n`);
        const refreshedLogs = await until(async () => {
          const state = await client.evaluate(logSnapshot);
          return state.newEventShown && state;
        }, 'periodic log refresh after new event', 12000);
        check(!refreshedLogs.hiddenProbeShown && refreshedLogs.bottomGap <= 2 && refreshedLogs.lastVisible,
          `periodic log refresh did not stay at the latest record: ${JSON.stringify(refreshedLogs)}`);
        log('periodic background log refresh shows the new record and stays at the bottom');
      }
      if (view === 'setup') {
        const nextState = await client.evaluate(`(() => {
          document.querySelector('#welcome-phone').value = 'layout-test-account';
          document.querySelector('#welcome-uid').value = 'layout-test-uid';
          document.querySelector('#first-run-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
          return { captureHidden: document.querySelector('#capture-modal').hidden, accountVisible: document.querySelector('#view-account').classList.contains('active') };
        })()`);
        check(nextState.captureHidden && nextState.accountVisible, 'first-run next step unexpectedly opened the capture window');
        log('first-run next step leaves manual ID entry available without opening capture');
      }
    }
    const failures = [];
    for (const result of results) {
      const { view, viewport, elements } = result;
      if (viewport.documentScrollWidth > viewport.documentClientWidth + 1 || viewport.bodyScrollWidth > viewport.documentClientWidth + 1)
        failures.push(`${view}: horizontal page overflow ${Math.max(viewport.documentScrollWidth, viewport.bodyScrollWidth)} > ${viewport.documentClientWidth}`);
      if (viewport.documentScrollHeight > viewport.height + 1 || viewport.bodyScrollHeight > viewport.height + 1)
        failures.push(`${view}: vertical page overflow ${Math.max(viewport.documentScrollHeight, viewport.bodyScrollHeight)} > ${viewport.height}`);
      for (const element of elements) {
        if (element.missing || !element.visible) { failures.push(`${view}: ${element.selector} is missing or hidden`); continue; }
        if (element.left < -1 || element.right > viewport.width + 1)
          failures.push(`${view}: ${element.selector} extends outside width ${viewport.width} (${element.left}..${element.right})`);
        if (element.top < -1 || element.bottom > viewport.height + 1)
          failures.push(`${view}: ${element.selector} extends outside height ${viewport.height} (${element.top}..${element.bottom})`);
        if (view === 'account' && ['#view-account #service-name', '#view-account #apartment-id', '#view-account #room-id'].includes(element.selector) && !element.editable)
          failures.push(`${view}: ${element.selector} is not editable for manual configuration`);
      }
    }
    check(failures.length === 0, failures.join('; '));
    log('PASS: all setup and main-view controls fit the default viewport');
  } finally {
    try { client?.close(); } catch {}
    if (child?.pid && child.exitCode === null) {
      try { execFileSync('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { timeout: 8000, windowsHide: true, stdio: 'ignore' }); } catch {}
    }
    try {
      check(dirname(resolve(root)) === resolve(tmpdir()) && resolve(root).split(/[\\/]/).at(-1).startsWith('campus-main-layout-'), 'refusing unsafe temporary profile cleanup');
      await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 250 });
    } catch (error) { log(`temporary profile retained: ${root} (${error.code || error.message})`); }
  }
}

run().catch(error => { console.error(`[main-layout] FAIL: ${error.stack || error}`); process.exitCode = 1; });
