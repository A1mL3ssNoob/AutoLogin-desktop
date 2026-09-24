/* Vanilla UI. All side effects remain in the native backend. No credentials in localStorage. */
(() => {
  'use strict';
  const $ = selector => document.querySelector(selector);
  const $$ = selector => [...document.querySelectorAll(selector)];
  const api = window.__TAURI__;
  const nativeInvoke = api?.core?.invoke || api?.invoke;
  const isDesktop = typeof nativeInvoke === 'function';
  const demo = () => !isDesktop;
  const invoke = (command, args = {}) => isDesktop ? nativeInvoke(command, args) : Promise.reject(new Error('网页预览模式不执行后台操作'));
  let config = null;
  let hasCredentials = false;
  let pendingCredentials = null;
  let status = { status: 'checking', detail: '', checking: false, last_check: null, last_success: null, last_error: null };
  let logs = [];
  let filter = 'all';
  let captureStarted = false;
  let captureGeneration = 0;
  let captureClosePromise = null;
  let captureTimer;
  let lastFocused;

  const logLabels = Object.freeze({
    configuration_changed: '配置已更新',
    probe_online: '网络正常', probe_failed: '探测失败', portal_not_found: '未发现认证门户',
    auth_attempt: '认证尝试', auth_success: '认证成功', auth_failed: '认证失败', auth_exhausted: '认证停止',
    auth_verify: '连接验证', auth_oauth_response: '收到 OAuth 响应', auth_code_received: '收到认证码', auth_final_request: '发送登录请求',
    capture_changed: '捕获状态', capture_success: '捕获成功', capture_window_opening: '打开认证窗口',
    capture_window_ready: '认证窗口已就绪', capture_window_closed: '认证窗口已关闭', capture_window_create_failed: '认证窗口创建失败',
    capture_window_close_failed: '关闭认证窗口失败', capture_config_read_failed: '读取捕获配置失败', capture_config_save_failed: '保存捕获配置失败',
    main_window_create_failed: '创建主窗口失败', auto_start_changed: '开机启动设置', pause_changed: '暂停设置',
    login_skipped: '跳过登录', login_cancelled: '取消登录'
  });

  function escapeHtml(value) { return String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch])); }
  // Names come only from the hand-drawn symbol set, never from backend data.
  function icon(name) { return `<svg class="icon" viewBox="0 0 24 24" aria-hidden="true" focusable="false"><use href="assets/icons.svg#${name}"></use></svg>`; }
  function setIcon(selector, name) { $(`${selector} use`).setAttribute('href', `assets/icons.svg#${name}`); }
  function notify(title, message = '', kind = 'success') { const el = document.createElement('div'); el.className = `toast ${kind}`; el.innerHTML = `<strong>${escapeHtml(title)}</strong><span>${escapeHtml(message)}</span>`; $('#toast-region').append(el); setTimeout(() => el.remove(), 6000); }
  function previewNotice() { notify('这是界面预览', '请启动桌面应用以执行认证、保存配置或导出日志。', 'error'); }
  function errorMessage(error) { const value = typeof error === 'string' ? error : error?.message; return value || '请查看运行日志了解详细原因。'; }
  function statusName() { return typeof status.status === 'string' ? status.status : Object.keys(status.status || {})[0]; }
  function parseTimestamp(value) {
    if (value instanceof Date) return Number.isNaN(value.getTime()) ? null : value;

    // The backend sends Unix epoch seconds as strings. Passing a numeric string
    // straight to Date makes it invalid (and used to leave the raw timestamp
    // visible in the card), so convert seconds explicitly. Also accept epoch
    // milliseconds and regular date strings for compatibility with old state.
    const text = typeof value === 'string' ? value.trim() : value;
    if (text === '' || text === null || text === undefined) return null;
    if ((typeof text === 'number' && Number.isFinite(text)) || /^-?\d+(?:\.\d+)?$/.test(text)) {
      const timestamp = Number(text);
      if (Number.isFinite(timestamp)) {
        const milliseconds = Math.abs(timestamp) < 1e11 ? timestamp * 1000 : timestamp;
        const date = new Date(milliseconds);
        if (!Number.isNaN(date.getTime())) return date;
      }
    }

    const date = new Date(text);
    return Number.isNaN(date.getTime()) ? null : date;
  }

  function displayTime(value) {
    const date = parseTimestamp(value);
    if (!date) return '暂无记录';
    const pad = part => String(part).padStart(2, '0');
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  }
  function showView(view) { $$('.view').forEach(el => el.classList.toggle('active', el.id === `view-${view}`)); $$('.nav-item').forEach(el => { const selected = el.dataset.view === view; el.classList.toggle('active', selected); if (selected) el.setAttribute('aria-current', 'page'); else el.removeAttribute('aria-current'); }); $('#page-title').textContent = {overview:'概览',account:'账号配置',logs:'运行日志'}[view]; if (view === 'logs') { scrollLogsToLatest(); refreshLogs(); } }
  function openModal(id) { lastFocused = document.activeElement; $(id).hidden = false; $(id).querySelector('input, button')?.focus(); }
  function hideModal(id) { $(id).hidden = true; lastFocused?.focus?.(); }

  function renderStatus() {
    const name = statusName();
    const titles = {
      setup_required: ['需要配置', '先完成校园网配置', '填写账号和楼栋/房间 ID，或使用自动获取后即可开始后台监测。', '未配置'],
      online: ['网络正常', '校园网已连接', '网络状态良好，应用会继续在后台监测。', '已连接'],
      checking: ['正在检测', '正在检查网络连接', '应用正在快速确认网络状态。', '检查中'],
      probe_failed: ['网络探测失败', '正在确认网络状态', '确认断开后会继续寻找校园认证门户并尝试恢复。', '探测失败'],
      portal_detected: ['需要认证', '发现校园认证页面', '已确认门户跳转，准备执行自动登录。', '待认证'],
      authenticating: ['正在认证', '正在登录校园网', '登录完成后会再次探测网络，确认连接恢复。', '认证中'],
      waiting_to_retry: ['等待重试', '正在等待下一次尝试', '登录失败后采用退避等待，避免频繁提交。', '等待重试'],
      offline: ['网络不可达', '暂时无法连接网络', '未发现校园门户，应用会继续探测网络。', '不可达'],
      paused: ['已暂停', '自动监测已暂停', '恢复监测后，应用会继续检查校园网状态。', '已暂停'],
      needs_attention: ['需要处理', '自动登录未能恢复网络', '请检查账号配置或查看日志了解失败原因。', '需处理']
    };
    const values = titles[name] || titles.checking;
    const detectingOnline = name === 'online' && status.checking;
    // During a confirmation probe the connection is still considered online.
    // Keep the green "网络正常" label and "校园网已连接" heading stable; only
    // the detail line below them changes to "正在检测" until the probe settles.
    $('#status-label').textContent = values[0];
    $('#status-heading').textContent = values[1];
    $('#status-description').textContent = detectingOnline ? '正在检测' : status.detail || values[2];
    $('#metric-network').textContent = values[3];
    $('#status-indicator').className = `status-indicator ${name === 'online' ? 'online' : name === 'paused' ? 'paused' : ''}`;
    $('#pause-label').textContent = name === 'paused' ? '恢复监测' : '暂停监测';
    setIcon('#pause-icon', name === 'paused' ? 'play' : 'pause');
    setIcon('#signal-icon', name === 'paused' ? 'pause' : ['offline', 'probe_failed', 'needs_attention'].includes(name) ? 'wifi-off' : 'wifi');
    $('.signal-visual').dataset.state = name;
    $('#metric-login').textContent = displayTime(status.last_success);
    $('#metric-next').textContent = status.last_check ? displayTime(status.last_check) : '尚未检测';
    $('#network-trend').textContent = name === 'online' ? '正常' : name === 'paused' ? '手动' : '自动';
    $('#login-trend').textContent = status.last_success ? '成功' : '—';
    $('.monitor-pill span:last-child').textContent = demo() ? '界面预览' : name === 'paused' ? '监测已暂停' : name === 'setup_required' ? '等待首次配置' : '后台监测中';
  }
  function renderConfig(view) {
    config = view.config; hasCredentials = Boolean(view.has_credentials);
    $('#apartment-id').value = config?.apartment_id || ''; $('#room-id').value = config?.room_id || '';
    $('#service-name').value = config?.service_name || 'chinaTelecom';
    $('#startup-toggle').checked = Boolean(config?.auto_start);
    $('#profile-label').textContent = hasCredentials ? '凭据已保存' : '未配置';
    $('#phone').placeholder = hasCredentials ? '重新输入账号以修改' : '请输入手机号或账号';
    $('#uid').placeholder = hasCredentials ? '重新输入 UID 以修改' : '请输入 UID 或认证码';
    $('#secure-label').textContent = hasCredentials ? '已保存' : '安全存储';
  }
  function scrollLogsToLatest() {
    const list = $('#log-list');
    if (!list) return;
    // The newest record is appended at the end of the list. Set the position
    // after rendering and once more after layout so entering the page and each
    // background refresh both land on the newest record.
    list.scrollTop = list.scrollHeight;
    requestAnimationFrame(() => { list.scrollTop = list.scrollHeight; });
  }
  function isUsefulLog(entry) { return entry?.label !== 'probe_online'; }
  function renderLogs() {
    const row = entry => `<div class="log-row"><span class="log-time">${escapeHtml(displayTime(entry.time))}</span><span class="log-kind ${['auth','error'].includes(entry.kind) ? entry.kind : 'network'}">${escapeHtml(logLabels[entry.label] || entry.label || entry.kind)}</span><span class="log-message">${escapeHtml(entry.message)}</span></div>`;
    const usefulLogs = logs.filter(isUsefulLog);
    const selected = usefulLogs.filter(entry => filter === 'all' || entry.kind === filter);
    $('#log-list').innerHTML = selected.length ? selected.map(row).join('') : `<div class="empty-state"><span class="empty-icon">${icon('logs')}</span><p>暂无日志</p><small>日志会在后台运行后显示</small></div>`;
    $('#activity-list').innerHTML = usefulLogs.length ? usefulLogs.slice(-3).reverse().map(row).join('') : `<div class="empty-state"><span class="empty-icon">${icon('activity')}</span><p>还没有运行记录</p><small>有用的状态变化会显示在这里</small></div>`;
    scrollLogsToLatest();
  }
  async function refreshLogs() { if (demo()) return; try { const result = await invoke('get_logs'); if (Array.isArray(result)) { logs = result; renderLogs(); } } catch (_) { /* The state panel stays available if diagnostics cannot be read. */ } }
  async function refresh(showError = false) { if (demo()) return; try { status = await invoke('get_status'); renderStatus(); await refreshLogs(); } catch (error) { if (showError) notify('刷新失败', errorMessage(error), 'error'); } }
  function currentConfigValue() {
    return {
      ...(config || {}),
      apartment_id: $('#apartment-id').value.trim(),
      room_id: $('#room-id').value.trim(),
      service_name: $('#service-name').value || config?.service_name || 'chinaTelecom'
    };
  }
  async function saveConfigOnly() {
    const result = await invoke('save_config', {configValue: currentConfigValue()});
    renderConfig(result);
    $('#save-state').textContent = '配置已保存';
    await refresh();
  }
  async function saveCredentials(credentials) {
    const configValue = currentConfigValue();
    const result = await invoke('save_setup', { configValue, credentials });
    renderConfig(result); $('#phone').value = ''; $('#uid').value = ''; $('#welcome-phone').value = ''; $('#welcome-uid').value = ''; pendingCredentials = null;
    $('#save-state').textContent = '配置已保存'; await refresh();
  }
  async function saveAccount(event) {
    event.preventDefault(); if (demo()) { previewNotice(); return; }
    const phone = $('#phone').value.trim(), uid = $('#uid').value.trim();
    // Existing users can change non-sensitive settings, such as the carrier,
    // without re-entering credentials that remain in Windows secure storage.
    if (!phone && !uid && hasCredentials) {
      if (!$('#apartment-id').value.trim() || !$('#room-id').value.trim()) {
        notify('请填写楼栋 / 公寓 ID 和房间 ID', '可以直接手动填写，也可以点击“自动获取认证信息”。', 'error');
        return;
      }
      try { await saveConfigOnly(); notify('配置已保存', '运营商和房间配置已更新。'); } catch (error) { notify('保存失败', errorMessage(error), 'error'); }
      return;
    }
    if (!phone || !uid) {
      notify('请填写账号和认证码', hasCredentials ? '修改凭据时需要同时填写账号和 UID/认证码；只修改运营商时可留空。' : '完成填写后才能保存配置。', 'error');
      return;
    }
    if (!$('#apartment-id').value.trim() || !$('#room-id').value.trim()) {
      pendingCredentials = {phone,uid};
      notify('请填写楼栋 / 公寓 ID 和房间 ID', '可以直接手动填写，也可以点击“自动获取认证信息”。', 'error');
      return;
    }
    try { await saveCredentials({phone,uid}); notify('配置已保存', '账号与认证码已加密保存在本机。'); } catch (error) { notify('保存失败', errorMessage(error), 'error'); }
  }
  function openCapture() { $('#capture-progress').hidden = !captureStarted; $('#start-capture-button').disabled = captureStarted; openModal('#capture-modal'); }
  function resetCapture() {
    clearTimeout(captureTimer); captureTimer = undefined; captureStarted = false; captureGeneration += 1;
    $('#capture-progress').hidden = true; $('#start-capture-button').disabled = false;
  }
  function captureWindowClosed(result) {
    // A cancelled window may finish closing while its replacement is waiting.
    if (captureClosePromise) return;
    const wasCapturing = captureStarted;
    resetCapture();
    if (wasCapturing && result?.reason === 'create_failed') notify('无法打开认证页面', '认证窗口创建失败，请查看运行日志后重试。', 'error');
  }
  async function closeCapture() {
    const wasCapturing = captureStarted;
    resetCapture(); hideModal('#capture-modal');
    if (!captureClosePromise && wasCapturing && isDesktop) {
      captureClosePromise = invoke('close_capture_window').catch(() => {}).finally(() => { captureClosePromise = null; });
    }
    await captureClosePromise;
  }
  async function startCapture() {
    if (demo()) { previewNotice(); return; }
    if (captureStarted) return;
    captureStarted = true; const generation = ++captureGeneration;
    $('#start-capture-button').disabled = true; $('#capture-progress').hidden = false; $('#capture-progress-text').textContent = captureClosePromise ? '正在等待上一个认证窗口关闭…' : '正在打开校园认证页面…';
    try {
      if (captureClosePromise) await captureClosePromise;
      if (generation !== captureGeneration) return;
      $('#capture-progress-text').textContent = '正在打开校园认证页面…';
      await invoke('open_capture_window');
      if (generation !== captureGeneration) return;
      $('#capture-progress-text').textContent = '请在认证窗口完成登录，正在等待所需参数…';
      clearTimeout(captureTimer); captureTimer = setTimeout(() => { $('#capture-progress-text').textContent = '尚未捕获参数。确认已拔插路由器并在认证页面完成一次登录；也可关闭认证窗口后重试。'; }, 90000);
    } catch (error) { if (generation !== captureGeneration) return; resetCapture(); notify('无法打开认证页面', errorMessage(error), 'error'); }
  }
  async function captureResult(result) {
    if (captureClosePromise || !captureStarted) return;
    if (!result?.apartment_id || !result?.room_id) return;
    resetCapture();
    // Keep a carrier selected in the form even when it has not been saved yet.
    // The capture backend persists only the IDs, so reading the config back
    // must not reset this unsaved UI choice.
    const selectedService = $('#service-name').value || config?.service_name || 'chinaTelecom';
    config = { ...config, apartment_id: result.apartment_id, room_id: result.room_id, service_name: selectedService };
    $('#apartment-id').value = result.apartment_id; $('#room-id').value = result.room_id;
    $('#capture-progress').hidden = true; $('#start-capture-button').disabled = false; hideModal('#capture-modal'); showView('account');
    try {
      if (pendingCredentials) await saveCredentials(pendingCredentials);
      else await saveConfigOnly();
      notify('认证信息已获取', '公寓 ID 和房间 ID 已自动填入，请完成门户中的登录。'); await refresh();
    } catch (error) { notify('参数已获取，但保存失败', errorMessage(error), 'error'); }
  }
  async function beginSetup(event) {
    event.preventDefault();
    const phone = $('#welcome-phone').value.trim(), uid = $('#welcome-uid').value.trim();
    if (!phone || !uid) { notify('请填写账号和认证码', '两项内容都不能为空。', 'error'); return; }
    pendingCredentials = {phone,uid};
    $('#phone').value = phone; $('#uid').value = uid;
    hideModal('#first-run-modal'); showView('account'); $('#apartment-id').focus();
    notify('请完成配置', '请手动填写楼栋 / 公寓 ID 和房间 ID，或点击“自动获取认证信息”。');
  }
  async function loginNow() { if (demo()) { previewNotice(); return; } try { await invoke('login_now'); await refresh(); } catch (error) { notify('暂时无法登录', errorMessage(error), 'error'); } }
  async function togglePause() { if (demo()) { previewNotice(); return; } try { await invoke('set_paused', {paused:statusName() !== 'paused'}); await refresh(); } catch (error) { notify('操作失败', errorMessage(error), 'error'); } }
  async function exportLogs() { if (demo()) { previewNotice(); return; } try { const path = await invoke('export_logs'); notify('诊断日志已导出', typeof path === 'string' ? path : '日志已保存。'); } catch (error) { notify('导出失败', errorMessage(error), 'error'); } }
  async function startupChanged(event) { if (demo()) { event.target.checked = false; previewNotice(); return; } const enabled = event.target.checked; try { await invoke('set_startup_enabled', {enabled}); config.auto_start = enabled; notify(enabled ? '已开启开机启动' : '已关闭开机启动'); } catch (error) { event.target.checked = !enabled; notify('设置失败', errorMessage(error), 'error'); } }

  async function init() {
    document.addEventListener('click', event => {
      const target = event.target.closest('[data-view], [data-action]'); if (!target) return;
      if (target.dataset.view) { showView(target.dataset.view); return; }
      const handlers = {capture:openCapture,'close-capture':closeCapture,'start-capture':startCapture,'login-now':loginNow,'toggle-pause':togglePause,refresh:()=>refresh(true),'export-logs':exportLogs,'show-help':()=>notify('获取认证信息', '楼栋 / 公寓 ID 和房间 ID 可以手动填写；如果不知道内部标识，请拔插路由器后使用“自动获取认证信息”。'),'toggle-secret':()=>{
        const input = $('#uid');
        const reveal = input.type === 'password';
        input.type = reveal ? 'text' : 'password';
        setIcon('#secret-icon', reveal ? 'eye-off' : 'eye');
        target.setAttribute('aria-label', reveal ? '隐藏认证码' : '显示认证码');
        target.setAttribute('title', reveal ? '隐藏认证码' : '显示认证码');
        target.setAttribute('aria-pressed', String(reveal));
      }};
      handlers[target.dataset.action]?.();
    });
    $('#account-form').addEventListener('submit', saveAccount); $('#first-run-form').addEventListener('submit', beginSetup); $('#startup-toggle').addEventListener('change', startupChanged);
    $$('.filter').forEach(button => button.addEventListener('click', () => { filter = button.dataset.logFilter; $$('.filter').forEach(item => item.classList.toggle('active',item===button)); renderLogs(); }));
    document.addEventListener('keydown', event => { const modal = $$('.modal-backdrop').find(el => !el.hidden); if (!modal) return; if (event.key === 'Escape' && modal.id === 'capture-modal') closeCapture(); if (event.key === 'Tab') { const elements = [...modal.querySelectorAll('button:not([disabled]),input:not([disabled])')]; const first=elements[0], last=elements.at(-1); if (event.shiftKey && document.activeElement === first) { last.focus(); event.preventDefault(); } else if (!event.shiftKey && document.activeElement === last) { first.focus(); event.preventDefault(); } } });
    if (api?.event?.listen) { await api.event.listen('status-changed', event => { status=event.payload; renderStatus(); }); await api.event.listen('capture-result', event => captureResult(event.payload)); await api.event.listen('capture-window-closed', event => captureWindowClosed(event.payload)); }
    if (isDesktop) {
      try { renderConfig(await invoke('get_config')); await refresh(); if (!hasCredentials || !config?.apartment_id || !config?.room_id) openModal('#first-run-modal'); } catch (error) { notify('无法读取应用配置', errorMessage(error), 'error'); }
      // Backend status events are authoritative and arrive at probe start/end.
      // Keep the periodic refresh for logs only so an older IPC status read
      // cannot overwrite a fresh "正在检测" event.
      setInterval(() => refreshLogs(), 5000);
    } else { status={...status,status:'setup_required',detail:'当前为界面预览。后台操作需要在桌面应用中运行。'}; renderStatus(); setTimeout(() => openModal('#first-run-modal'), 260); }
  }
  document.addEventListener('DOMContentLoaded', init);
})();
