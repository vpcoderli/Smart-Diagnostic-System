// ═══════════════════════════════════════════════════
//  Smart Diagnostic Collector — app.js
//  Obsidian dark theme + premium glassmorphism
// ═══════════════════════════════════════════════════

// ─── Tauri invoke bridge ───
const invoke = window.__TAURI__?.core?.invoke
  || (async (cmd, args) => { throw new Error(`Tauri not available (cmd: ${cmd})`); });

// ─── App State ───
const state = {
  // Navigation
  currentStep: 1,

  // Phase 1: 通用配置
  scheduleEnabled: false,

  // Phase 2: 日志配置
  logSource: 'elk',        // 当前「选中查看」的配置面板：'elk' | 'es' | 'ssh'
  activeSource: null,      // 「测试通过并激活」的日志源，诊断实际使用的就是它（null=尚未激活）
  svcImported: false,
  elkAddressFilled: false,
  elkValidated: false,
  esAddressFilled: false,
  esValidated: false,
  sshValidated: false,
  discoveredEsConfig: null,
  discoveredEsConfigDirect: null,

  // Phase 3: 数据库与模式
  dbType: 'mysql',         // 'mysql' | 'postgresql'
  dbValidated: false,
  availableTables: [],

  // Phase 4: 诊断采集
  diagMode: 'realtime',    // 'realtime' | 'history' | 'scheduler' | 'quick'
  diagBrowserOpen: false,
  capturedRequests: [],
  validationConfirmed: false,

  // Timers
  countPollingTimer: null,
  schedulerPollingTimer: null,
};

// ─── DOMContentLoaded ───
document.addEventListener('DOMContentLoaded', async () => {
  initNavigation();
  initPhase1();
  initPhase2();
  initPhase3();
  initPhase4();
  setDefaultTimeRange();

  // 清空配置按钮
  document.getElementById('btn-clear-config').addEventListener('click', clearAllConfig);

  // 一键直达 ZIP 目录及复制路径绑定
  setupResultActionButtons();

  // 尝试加载配置，失败则预填桌面路径
  const loaded = await tryLoadSavedConfig();
  if (!loaded) {
    prefillOutputPath();
  }

  // 监听 Tauri 调度器事件：创建了新诊断包
  if (window.__TAURI__?.event?.listen) {
    window.__TAURI__.event.listen('scheduler-package-created', (ev) => {
      const p = ev.payload || {};
      showToast(`定时巡检生成新诊断包：${p.traceCount ?? '?'} 个 traceId`, 'success', 5000);
      refreshSchedulerPackageList(p);
    });
  }
});

// ═══════════════════════════════════════════════════
//  Navigation Control (双向跳转)
// ═══════════════════════════════════════════════════

function initNavigation() {
  document.querySelectorAll('.step-item').forEach(item => {
    item.addEventListener('click', async () => {
      const targetStep = parseInt(item.dataset.step, 10);
      if (await canNavigateTo(targetStep)) {
        await jumpToStep(targetStep);
      }
    });
  });
}

async function canNavigateTo(targetStep) {
  const current = state.currentStep;
  if (targetStep <= current) return true; // 允许随时回退

  // 校验第一步：必须填写站点名称
  if (targetStep > 1) {
    const siteName = document.getElementById('site-name').value.trim();
    if (!siteName) {
      showToast('请输入站点名称', 'error');
      return false;
    }
  }

  // 校验第二步：按当前激活的日志源分别校验（ELK 地址 / ES 地址 / SSH 服务导入）
  if (targetStep > 2) {
    if (state.logSource === 'elk') {
      if (!document.getElementById('elk-address').value.trim()) {
        showToast('请输入 ELK 地址', 'error');
        return false;
      }
    } else if (state.logSource === 'es') {
      if (!document.getElementById('es-address').value.trim()) {
        showToast('请输入 ES 地址', 'error');
        return false;
      }
    } else { // ssh
      if (!state.svcImported) {
        showToast('请先导入服务部署 CSV 配置', 'error');
        return false;
      }
    }
  }

  // 校验第三步：必须填写数据库地址和数据库名称
  if (targetStep > 3) {
    const dbHost = document.getElementById('db-host').value.trim();
    const dbName = document.getElementById('db-name').value.trim();
    if (!dbHost || !dbName) {
      showToast('请配置数据库并选定具体业务库', 'error');
      return false;
    }
  }

  return true;
}

async function jumpToStep(step) {
  // 切走之前自动保存当前输入
  await saveStepConfig(state.currentStep);

  state.currentStep = step;
  showPhase(step);

  if (step === 2) {
    populateVerifyAddresses();
  } else if (step === 3) {
    populateDbVerifyAddr();
  } else if (step === 4) {
    await preparePhase4();
  }
}

function showPhase(n) {
  [1, 2, 3, 4].forEach(i => {
    const phase = document.getElementById('phase-' + i);
    const nav   = document.getElementById('nav-step-' + i);
    const num   = document.getElementById('step-num-' + i);

    if (phase) phase.classList.toggle('active', i === n);
    if (nav) {
      nav.classList.remove('active', 'done');
      if (i === n) {
        nav.classList.add('active');
        num.innerHTML = String(i);
      } else if (i < n) {
        nav.classList.add('done');
        num.innerHTML = `<svg class="icon icon-sm" aria-hidden="true" style="color:#fff;"><use href="#icon-check"/></svg>`;
      } else {
        num.innerHTML = String(i);
      }
    }
  });
}

// 对应步骤的数据保存
async function saveStepConfig(step) {
  if (step === 1) {
    const site = document.getElementById('site-name').value.trim();
    const gateway = document.getElementById('gateway-prefix').value.trim() || '/gateway';
    if (site) {
      try {
        await invoke('set_site_info', { siteName: site, gatewayPrefix: gateway });
        await invoke('set_schedule_config', { schedule: buildScheduleConfig() });
      } catch (e) { console.warn('保存第一步配置失败:', e); }
    }
  } else if (step === 2) {
    // 无论当前激活哪种日志源，只要地址非空就两个都保存，
    // 防止切换模式时另一种配置丢失
    const elkAddr = document.getElementById('elk-address')?.value.trim() || '';
    if (elkAddr) {
      try {
        await invoke('set_elk_config', { elk: buildElkConfig() });
      } catch (e) { console.warn('保存 ELK 配置失败:', e); }
    }
    const esAddr = document.getElementById('es-address')?.value.trim() || '';
    if (esAddr) {
      try {
        await invoke('set_es_config', { es: buildEsConfig() });
      } catch (e) { console.warn('保存 ES 配置失败:', e); }
    }
  } else if (step === 3) {
    const dbHost = document.getElementById('db-host')?.value.trim() || '';
    if (dbHost) {
      try {
        await invoke('import_db_csv', { csvContent: buildDbCsv() });
      } catch (e) { console.warn('保存数据库配置失败:', e); }
    }
  }
  await autoSaveConfig();
}

// ═══════════════════════════════════════════════════
//  Phase 1 — 通用信息
// ═══════════════════════════════════════════════════

function initPhase1() {
  document.getElementById('site-name').addEventListener('input', checkPhase1Ready);
  document.getElementById('btn-browse-path').addEventListener('click', browseOutputPath);

  document.getElementById('schedule-enabled').addEventListener('change', e => {
    state.scheduleEnabled = e.target.checked;
    document.getElementById('schedule-fields').style.display = e.target.checked ? 'block' : 'none';
  });

  document.getElementById('btn-goto-phase2').addEventListener('click', () => jumpToStep(2));
}

function checkPhase1Ready() {
  const siteOk = document.getElementById('site-name').value.trim().length > 0;
  document.getElementById('btn-goto-phase2').disabled = !siteOk;
}

// ═══════════════════════════════════════════════════
//  Phase 2 — 日志配置
// ═══════════════════════════════════════════════════

function initPhase2() {
  document.getElementById('source-elk').addEventListener('click', () => setLogSource('elk'));
  document.getElementById('source-elk').addEventListener('keydown', e => e.key === 'Enter' && setLogSource('elk'));
  document.getElementById('source-es').addEventListener('click', () => setLogSource('es'));
  document.getElementById('source-es').addEventListener('keydown', e => e.key === 'Enter' && setLogSource('es'));
  document.getElementById('source-ssh').addEventListener('click', () => setLogSource('ssh'));
  document.getElementById('source-ssh').addEventListener('keydown', e => e.key === 'Enter' && setLogSource('ssh'));

  document.getElementById('elk-address').addEventListener('input', () => {
    state.elkAddressFilled = document.getElementById('elk-address').value.trim().length > 0;
    populateVerifyAddresses();
    checkPhase2Ready();
  });

  document.getElementById('es-address').addEventListener('input', () => {
    state.esAddressFilled = document.getElementById('es-address').value.trim().length > 0;
    populateVerifyAddresses();
    checkPhase2Ready();
  });

  setupPasswordEye('elk-password', 'btn-elk-eye', 'elk-eye-icon');
  setupPasswordEye('es-password', 'btn-es-eye', 'es-eye-icon');

  document.getElementById('elk-adv-toggle').addEventListener('click', () => {
    toggleAdvanced('elk-adv-toggle', 'elk-adv-content');
  });
  document.getElementById('es-adv-toggle').addEventListener('click', () => {
    toggleAdvanced('es-adv-toggle', 'es-adv-content');
  });

  // SSH CSV file drop
  const svcDrop = document.getElementById('svc-drop');
  svcDrop.addEventListener('click', () => document.getElementById('svc-file').click());
  svcDrop.addEventListener('keydown', e => e.key === 'Enter' && document.getElementById('svc-file').click());
  svcDrop.addEventListener('dragover', e => { e.preventDefault(); svcDrop.style.borderColor = 'var(--accent)'; });
  svcDrop.addEventListener('dragleave', () => { svcDrop.style.borderColor = ''; });
  svcDrop.addEventListener('drop', e => {
    e.preventDefault();
    svcDrop.style.borderColor = '';
    const file = e.dataTransfer?.files[0];
    if (file) handleSvcFile(file);
  });
  document.getElementById('svc-file').addEventListener('change', e => {
    if (e.target.files[0]) handleSvcFile(e.target.files[0]);
    e.target.value = '';
  });

  document.getElementById('btn-dl-svc-tpl').addEventListener('click', downloadTemplate);

  // Validation buttons
  document.getElementById('btn-test-elk').addEventListener('click', testElk);
  document.getElementById('btn-discover-es').addEventListener('click', discoverEsConfig);
  document.getElementById('btn-apply-es-config').addEventListener('click', applyEsConfig);
  document.getElementById('btn-test-es').addEventListener('click', testEs);
  document.getElementById('btn-discover-es-direct').addEventListener('click', discoverEsConfigDirect);
  document.getElementById('btn-apply-es-config-direct').addEventListener('click', applyEsConfigDirect);
  document.getElementById('btn-validate-svc').addEventListener('click', validateSvc);

  document.getElementById('btn-backto-phase1').addEventListener('click', () => jumpToStep(1));
  document.getElementById('btn-goto-phase3').addEventListener('click', () => jumpToStep(3));
}

function setLogSource(src) {
  state.logSource = src;
  document.getElementById('source-elk').classList.toggle('selected', src === 'elk');
  document.getElementById('source-es').classList.toggle('selected', src === 'es');
  document.getElementById('source-ssh').classList.toggle('selected', src === 'ssh');
  document.getElementById('elk-config-area').style.display = src === 'elk' ? 'block' : 'none';
  document.getElementById('elk-verify-card').style.display = src === 'elk' ? 'block' : 'none';
  document.getElementById('es-config-area').style.display = src === 'es' ? 'block' : 'none';
  document.getElementById('es-verify-card').style.display = src === 'es' ? 'block' : 'none';
  document.getElementById('ssh-config-area').style.display = src === 'ssh' ? 'block' : 'none';
  document.getElementById('ssh-verify-card').style.display = src === 'ssh' ? 'block' : 'none';
  
  // 快速诊断的字段配置均已移至全局配置页，诊断界面不再包含配置信息

  populateVerifyAddresses();
  checkPhase2Ready();
  // 注意：仅「选中」不代表「激活」。激活由 setActiveSource 在连接测试通过后触发。
}

/**
 * 标记某个日志源为「已激活」（测试通过后调用）。
 * 全局只允许一个激活源；激活后才会显示「✓ 已激活」徽标，并作为诊断实际使用的日志源持久化。
 */
function setActiveSource(src) {
  state.activeSource = src;
  ['elk', 'es', 'ssh'].forEach(s => {
    document.getElementById('source-' + s).classList.toggle('activated', s === src);
  });
  // 持久化激活的日志来源——诊断、定时巡检、重启恢复都以它为准
  invoke('set_log_source', { logSource: src }).catch(e => {
    console.debug('日志来源暂时不能持久化:', e);
  });
}

/** 撤销激活状态。仅当被撤销的正是当前激活源时才清除（测试失败/配置变更时调用）。 */
function clearActiveSource(src) {
  if (src && state.activeSource !== src) return;
  state.activeSource = null;
  ['elk', 'es', 'ssh'].forEach(s => {
    document.getElementById('source-' + s).classList.remove('activated');
  });
}

/** 诊断入口的统一校验：必须先有一个「测试通过并激活」的日志源。返回激活源或 null。 */
function requireActiveSource() {
  if (!state.activeSource) {
    showToast('请先在「日志配置」中测试并激活一个日志源（测试通过后才会标记为已激活）', 'error', 5000);
    return null;
  }
  return state.activeSource;
}

function checkPhase2Ready() {
  const logOk = (state.logSource === 'elk' && state.elkAddressFilled)
             || (state.logSource === 'es' && state.esAddressFilled)
             || (state.logSource === 'ssh' && state.svcImported);
  document.getElementById('btn-goto-phase3').disabled = !logOk;
}

function populateVerifyAddresses() {
  const elkAddr = document.getElementById('elk-address').value.trim();
  document.getElementById('elk-verify-addr').textContent = elkAddr || '未配置';
  const esAddr = document.getElementById('es-address').value.trim();
  document.getElementById('es-verify-addr').textContent = esAddr || '未配置';
}

async function testElk() {
  const btn = document.getElementById('btn-test-elk');
  btn.disabled = true;
  setVerifyStatus('elk', 'testing', '测试中...');

  try {
    const elk = buildElkConfig();
    const result = await invoke('test_elk_connection', {
      address:      elk.address,
      indexPattern: elk.indexPattern,
      username:     elk.username,
      password:     elk.password,
    });
    setVerifyStatus('elk', 'ok', '连接成功', `ES ${result.esVersion || '未知版本'} — 连接成功`, 'ok');
    state.elkValidated = true;
    setActiveSource('elk');
    showToast('ELK 连接测试成功，已激活为当前日志源', 'success');
  } catch (err) {
    setVerifyStatus('elk', 'fail', '连接失败', String(err), 'fail');
    state.elkValidated = false;
    clearActiveSource('elk');
    showToast('ELK 连接失败: ' + err, 'error');
  }

  btn.disabled = false;
}

async function testEs() {
  const btn = document.getElementById('btn-test-es');
  btn.disabled = true;
  setVerifyStatus('es', 'testing', '测试中...');

  try {
    const es = buildEsConfig();
    const result = await invoke('test_es_connection', {
      address:      es.address,
      indexPattern: es.indexPattern,
      username:     es.username,
      password:     es.password,
    });
    setVerifyStatus('es', 'ok', '连接成功', `ES ${result.esVersion || '未知版本'} — 连接成功`, 'ok');
    state.esValidated = true;
    setActiveSource('es');
    showToast('ES 连接测试成功，已激活为当前日志源', 'success');
  } catch (err) {
    setVerifyStatus('es', 'fail', '连接失败', String(err), 'fail');
    state.esValidated = false;
    clearActiveSource('es');
    showToast('ES 连接失败: ' + err, 'error');
  }

  btn.disabled = false;
}

async function discoverEsConfigDirect() {
  const btn = document.getElementById('btn-discover-es-direct');
  const panel = document.getElementById('es-discover-panel-direct');
  const fieldsDiv = document.getElementById('es-discover-fields-direct');
  const servicesListDiv = document.getElementById('es-discover-services-list-direct');

  btn.disabled = true;
  panel.style.display = 'none';
  setVerifyStatus('es', 'testing', '正在自动探测...');

  try {
    const es = buildEsConfig();
    const result = await invoke('discover_log_config_from_es', {
      address:      es.address,
      indexPattern: es.indexPattern,
      username:     es.username,
      password:     es.password,
    });

    state.discoveredEsConfigDirect = result;
    setVerifyStatus('es', 'ok', '探测成功', `探测成功，共发现 ${result.services.length} 个活跃微服务。`, 'ok');
    showToast('ES 智能探查成功', 'success');

    const mapping = result.fieldMapping;
    fieldsDiv.innerHTML = `
      <div style="margin-bottom: 4px;"><strong>时间戳字段</strong>: <code>${mapping.timestamp}</code></div>
      <div style="margin-bottom: 4px;"><strong>日志级别字段</strong>: <code>${mapping.level}</code></div>
      <div style="margin-bottom: 4px;"><strong>TraceId 字段</strong>: <code>${mapping.traceId}</code></div>
      <div style="margin-bottom: 4px;"><strong>服务名字段</strong>: <code>${mapping.service}</code></div>
      <div style="margin-bottom: 0px;"><strong>消息字段</strong>: <code>${mapping.message}</code></div>
    `;

    if (result.services.length === 0) {
      servicesListDiv.innerHTML = '<div style="color:var(--text-3); font-size:12px; grid-column: 1 / -1;">未探测到任何活跃微服务日志。</div>';
    } else {
      servicesListDiv.innerHTML = result.services.map(name => `
        <label style="display:flex; align-items:center; gap:6px; color:var(--text-2); cursor:pointer;">
          <input type="checkbox" name="es-discover-svc-direct" value="${name}" checked />
          <span>${name}</span>
        </label>
      `).join('');
    }

    panel.style.display = 'block';
  } catch (err) {
    setVerifyStatus('es', 'fail', '探测失败', String(err), 'fail');
    showToast('ES 探测失败: ' + err, 'error');
  }

  btn.disabled = false;
}

async function applyEsConfigDirect() {
  if (!state.discoveredEsConfigDirect) return;

  const btn = document.getElementById('btn-apply-es-config-direct');
  btn.disabled = true;

  try {
    const checkedBoxes = document.querySelectorAll('input[name="es-discover-svc-direct"]:checked');
    const selectedServices = Array.from(checkedBoxes).map(cb => cb.value);

    const mapping = state.discoveredEsConfigDirect.fieldMapping;
    document.getElementById('es-field-timestamp').value = mapping.timestamp;
    document.getElementById('es-field-level').value = mapping.level;
    document.getElementById('es-field-traceid').value = mapping.traceId;
    document.getElementById('es-field-service').value = mapping.service;
    document.getElementById('es-field-message').value = mapping.message;

    const es = buildEsConfig();
    await invoke('set_es_config', { es });

    state.esAddressFilled = true;

    showToast(`配置应用成功！已激活 ${selectedServices.length} 个服务日志字段映射。`, 'success');
    checkPhase2Ready();

    await invoke('save_config_to_disk', {});
  } catch (err) {
    showToast('保存 ES 配置失败: ' + err, 'error');
  }

  btn.disabled = false;
}

async function discoverEsConfig() {
  const btn = document.getElementById('btn-discover-es');
  const panel = document.getElementById('es-discover-panel');
  const fieldsDiv = document.getElementById('es-discover-fields');
  const servicesListDiv = document.getElementById('es-discover-services-list');

  btn.disabled = true;
  panel.style.display = 'none';
  setVerifyStatus('elk', 'testing', '正在自动探测...');

  try {
    const elk = buildElkConfig();
    const result = await invoke('discover_log_config_from_es', {
      address:      elk.address,
      indexPattern: elk.indexPattern,
      username:     elk.username,
      password:     elk.password,
    });

    state.discoveredEsConfig = result;
    setVerifyStatus('elk', 'ok', '探测成功', `探测成功，共发现 ${result.services.length} 个活跃微服务。`, 'ok');
    showToast('ES 智能探查成功', 'success');

    // 展示字段探测报告
    const mapping = result.fieldMapping;
    fieldsDiv.innerHTML = `
      <div style="margin-bottom: 4px;"><strong>时间戳字段</strong>: <code>${mapping.timestamp}</code></div>
      <div style="margin-bottom: 4px;"><strong>日志级别字段</strong>: <code>${mapping.level}</code></div>
      <div style="margin-bottom: 4px;"><strong>TraceId 字段</strong>: <code>${mapping.traceId}</code></div>
      <div style="margin-bottom: 4px;"><strong>服务名字段</strong>: <code>${mapping.service}</code></div>
      <div style="margin-bottom: 0px;"><strong>消息字段</strong>: <code>${mapping.message}</code></div>
    `;

    // 展示服务复选框
    if (result.services.length === 0) {
      servicesListDiv.innerHTML = '<div style="color:var(--text-3); font-size:12px; grid-column: 1 / -1;">未探测到任何活跃微服务日志。</div>';
    } else {
      servicesListDiv.innerHTML = result.services.map(name => `
        <label style="display:flex; align-items:center; gap:6px; color:var(--text-2); cursor:pointer;">
          <input type="checkbox" name="es-discover-svc" value="${name}" checked />
          <span>${name}</span>
        </label>
      `).join('');
    }

    panel.style.display = 'block';
  } catch (err) {
    setVerifyStatus('elk', 'fail', '探测失败', String(err), 'fail');
    showToast('ES 探测失败: ' + err, 'error');
  }

  btn.disabled = false;
}

async function applyEsConfig() {
  if (!state.discoveredEsConfig) return;

  const btn = document.getElementById('btn-apply-es-config');
  btn.disabled = true;

  try {
    // 1. 获取选中的服务
    const checkedBoxes = document.querySelectorAll('input[name="es-discover-svc"]:checked');
    const selectedServices = Array.from(checkedBoxes).map(cb => cb.value);

    // 2. 回填字段映射到 UI 高级字段输入框中
    const mapping = state.discoveredEsConfig.fieldMapping;
    document.getElementById('elk-field-timestamp').value = mapping.timestamp;
    document.getElementById('elk-field-level').value = mapping.level;
    document.getElementById('elk-field-traceid').value = mapping.traceId;
    document.getElementById('elk-field-service').value = mapping.service;
    document.getElementById('elk-field-message').value = mapping.message;

    // 3. 将配置保存回后端 manifest 中
    const elk = buildElkConfig();
    await invoke('set_elk_config', { elk });

    // 4. 更新前端状态
    state.svcImported = true;
    state.elkAddressFilled = true;

    showToast(`配置应用成功！已激活 ${selectedServices.length} 个服务日志字段映射。`, 'success');
    checkPhase2Ready();

    // 自动保存至磁盘
    await invoke('save_config_to_disk', {});

    // 隐藏探测结果卡片
    document.getElementById('es-discover-panel').style.display = 'none';
  } catch (err) {
    showToast('应用配置失败: ' + err, 'error');
  }

  btn.disabled = false;
}

async function validateSvc() {
  const btn = document.getElementById('btn-validate-svc');
  btn.disabled = true;
  setVerifyStatus('ssh', 'testing', '校验中...');

  const listEl = document.getElementById('ssh-verify-list');
  listEl.innerHTML = '<div class="status-row"><span class="spinner"></span><span style="font-size:12px;color:var(--text-2)">正在连接服务器...</span></div>';

  try {
    const results = await invoke('validate_services', {});
    const allOk = Array.isArray(results) ? results.every(r => r.success) : true;
    listEl.innerHTML = Array.isArray(results)
      ? results.map(r => `
          <div class="status-row" style="margin-bottom:4px">
            <span class="status-dot ${r.success ? 'ok' : 'fail'}"></span>
            <span style="font-size:12px;color:var(--text-2)">${r.name || ''}</span>
            <span style="font-size:11px;color:${r.success ? 'var(--success)' : 'var(--error)'}">
              ${r.message || (r.success ? 'SSH 连通' : '失败')}
            </span>
          </div>`).join('')
      : '';
    setVerifyStatus('ssh', allOk ? 'ok' : 'fail', allOk ? '全部通过' : '部分失败');
    state.sshValidated = allOk;
    if (allOk) {
      setActiveSource('ssh');
      showToast('所有服务 SSH 校验通过，已激活为当前日志源', 'success');
    } else {
      clearActiveSource('ssh');
      showToast('部分服务 SSH 校验失败，请检查', 'error');
    }
  } catch (err) {
    listEl.innerHTML = `<span style="font-size:12px;color:var(--error)">${err}</span>`;
    setVerifyStatus('ssh', 'fail', '服务校验失败');
    state.sshValidated = false;
    clearActiveSource('ssh');
    showToast('服务校验失败: ' + err, 'error');
  }

  btn.disabled = false;
}

// ═══════════════════════════════════════════════════
//  Phase 3 — 数据库与模式
// ═══════════════════════════════════════════════════

function initPhase3() {
  document.querySelectorAll('#db-tab-bar .tab-btn').forEach(btn => {
    btn.addEventListener('click', () => setDbType(btn.dataset.dbType));
  });

  setupPasswordEye('db-password', 'btn-db-eye', 'db-eye-icon');
  document.getElementById('btn-test-db').addEventListener('click', testDb);

  // 搜索过滤表列表
  document.getElementById('db-tables-search').addEventListener('input', () => {
    renderTablePreview(state.availableTables);
  });

  document.getElementById('btn-backto-phase2').addEventListener('click', () => jumpToStep(2));
  document.getElementById('btn-goto-phase4').addEventListener('click', () => jumpToStep(4));
}

function setDbType(type) {
  state.dbType = type;
  document.querySelectorAll('#db-tab-bar .tab-btn').forEach(btn => {
    btn.classList.toggle('active', btn.dataset.dbType === type);
  });
  document.getElementById('db-port').value = type === 'postgresql' ? '5432' : '3306';
  hideSchemaRow();
  document.getElementById('db-tables-preview-card').style.display = 'none';
  state.dbValidated = false;
  checkPhase3Ready();
}

function populateDbVerifyAddr() {
  const dbHost = document.getElementById('db-host').value.trim();
  const dbPort = document.getElementById('db-port').value.trim();
  const dbName = document.getElementById('db-name').value.trim();
  document.getElementById('db-verify-addr').textContent =
    dbHost ? `${state.dbType}://${dbHost}:${dbPort}/${dbName}` : '未配置';
}

function checkPhase3Ready() {
  document.getElementById('btn-goto-phase4').disabled = !state.dbValidated;
}

async function testDb() {
  const btn = document.getElementById('btn-test-db');
  btn.disabled = true;
  setVerifyStatus('db', 'testing', '测试中...');

  // 触发一次临时保存，将当前的表单框配置同步到 Rust 端 manifest 状态
  const host = document.getElementById('db-host').value.trim();
  if (!host) {
    setVerifyStatus('db', 'fail', '请输入主机地址', '请输入主机地址', 'fail');
    btn.disabled = false;
    return;
  }
  try {
    await invoke('import_db_csv', { csvContent: buildDbCsv() });
  } catch (e) {
    console.warn('测试前同步数据库配置失败:', e);
  }

  try {
    const results = await invoke('validate_databases', {});
    const allOk = Array.isArray(results) ? results.every(r => r.success) : results?.success;
    if (allOk) {
      setVerifyStatus('db', 'ok', '连接成功', '数据库连接成功', 'ok');
      showToast('数据库连接测试成功', 'success');

      // 自动拉取数据库列表
      await populateDatabaseList();
      state.dbValidated = !!document.getElementById('db-name').value.trim();
    } else {
      const msg = Array.isArray(results) ? results.find(r => !r.success)?.message : results?.message;
      setVerifyStatus('db', 'fail', '连接失败', msg || '连接失败', 'fail');
      state.dbValidated = false;
      showToast('数据库连接失败: ' + (msg || ''), 'error');
    }
  } catch (err) {
    setVerifyStatus('db', 'fail', '连接失败', String(err), 'fail');
    state.dbValidated = false;
    showToast('数据库测试异常: ' + err, 'error');
  }

  btn.disabled = false;
  checkPhase3Ready();
}

async function populateDatabaseList() {
  const input = document.getElementById('db-name');
  const select = document.getElementById('db-name-select');
  const hint = document.getElementById('db-name-hint');
  if (!input || !select) return;

  try {
    const list = await invoke('list_available_databases', {});
    if (!Array.isArray(list) || list.length === 0) {
      hint.textContent = '提示：未获取到数据库列表，请手动填写数据库名';
      return;
    }
    const currentVal = input.value.trim();
    select.innerHTML = '<option value="">请选择数据库...</option>' +
      list.map(name => {
        const sel = (name === currentVal) ? ' selected' : '';
        return `<option value="${name}"${sel}>${name}</option>`;
      }).join('');

    input.style.display = 'none';
    select.style.display = '';
    hint.textContent = `检测到 ${list.length} 个数据库，请选择具体业务库`;
    hint.style.color = '#1976d2';

    select.onchange = async () => {
      const v = select.value;
      input.value = v;
      if (v) {
        try {
          await invoke('set_selected_database', { database: v });
          await invoke('import_db_csv', { csvContent: buildDbCsv() });
          hint.textContent = `已选定数据库：${v}`;
          hint.style.color = '#1b5e20';

          if (state.dbType === 'postgresql') {
            await populateSchemaList();
          } else {
            hideSchemaRow();
            state.dbValidated = true;
            await loadTablesList(); // MySQL 直接载入表
          }
        } catch (e) {
          hint.textContent = `保存选择失败: ${e}`;
          hint.style.color = '#c62828';
        }
      } else {
        state.dbValidated = false;
        hideSchemaRow();
        document.getElementById('db-tables-preview-card').style.display = 'none';
      }
      checkPhase3Ready();
    };

    if (currentVal && list.includes(currentVal)) {
      select.value = currentVal;
      select.dispatchEvent(new Event('change'));
    }
  } catch (e) {
    hint.textContent = `获取数据库列表失败: ${e}`;
    hint.style.color = '#c62828';
  }
}

async function populateSchemaList() {
  const row = document.getElementById('db-schema-row');
  const list = document.getElementById('db-schema-list');
  const hint = document.getElementById('db-schema-hint');
  if (!row || !list) return;

  try {
    const schemas = await invoke('list_available_schemas', {});
    if (!Array.isArray(schemas) || schemas.length === 0) {
      row.style.display = '';
      list.innerHTML = '<div style="color:#c62828;font-size:12px;">未获取到 schema 列表</div>';
      return;
    }
    row.style.display = '';
    list.innerHTML = schemas.map((name, idx) => `
      <label style="display:flex;align-items:center;padding:4px 2px;cursor:pointer;">
        <input type="checkbox" class="db-schema-cb" value="${name}" data-idx="${idx}" style="margin-right:8px;"/>
        <span style="font-size:13px;color:var(--text-1)">${name}</span>
      </label>
    `).join('');
    hint.textContent = `检测到 ${schemas.length} 个 schema，请勾选业务表所在的 schema（可多选）`;
    hint.style.color = '#1976d2';

    const onChange = async () => {
      const selected = Array.from(list.querySelectorAll('.db-schema-cb:checked'))
        .map(cb => cb.value);
      if (selected.length === 0) {
        state.dbValidated = false;
        hint.textContent = '请至少勾选一个 schema';
        hint.style.color = '#c62828';
        document.getElementById('db-tables-preview-card').style.display = 'none';
      } else {
        try {
          await invoke('set_selected_schemas', { schemas: selected });
          state.dbValidated = true;
          hint.textContent = `已选 ${selected.length} 个 schema：${selected.join(', ')}`;
          hint.style.color = '#1b5e20';
          await loadTablesList(); // 重新加载 PG 选定 schema 下的表
        } catch (e) {
          hint.textContent = `保存 schema 失败: ${e}`;
          hint.style.color = '#c62828';
          state.dbValidated = false;
        }
      }
      checkPhase3Ready();
    };

    list.querySelectorAll('.db-schema-cb').forEach(cb => {
      cb.addEventListener('change', onChange);
    });

    // 默认勾选第一个（通常是 public 等业务模式）
    const firstCb = list.querySelector('.db-schema-cb');
    if (firstCb) {
      firstCb.checked = true;
      onChange();
    }
  } catch (e) {
    row.style.display = '';
    list.innerHTML = `<div style="color:#c62828;font-size:12px;">获取 schema 列表失败: ${e}</div>`;
  }
}

async function loadTablesList() {
  const previewCard = document.getElementById('db-tables-preview-card');
  const previewList = document.getElementById('db-tables-preview-list');
  if (!previewCard || !previewList) return;

  previewCard.style.display = 'block';
  previewList.innerHTML = '<div style="color:var(--text-3); font-size:12px;"><span class="spinner" style="width:12px;height:12px;margin-right:6px"></span>正在拉取表结构元数据...</div>';

  try {
    const selectedSchemas = Array.from(document.querySelectorAll('.db-schema-cb:checked'))
      .map(cb => cb.value);
    
    // 调用新增的后端 Tauri 命令列出表
    const tables = await invoke('list_available_tables', { schemas: selectedSchemas });
    state.availableTables = tables || [];
    renderTablePreview(tables);
  } catch (err) {
    previewList.innerHTML = `<div style="color:var(--error); font-size:12px;">拉取表列表失败: ${err}</div>`;
  }
}

function renderTablePreview(tables) {
  const previewList = document.getElementById('db-tables-preview-list');
  if (!previewList) return;

  if (!Array.isArray(tables) || tables.length === 0) {
    previewList.innerHTML = '<div style="color:var(--text-3); font-size:12px;">该库或模式下未找到任何物理表</div>';
    return;
  }

  const query = document.getElementById('db-tables-search').value.trim().toLowerCase();
  const filtered = tables.filter(t => !query || t.toLowerCase().includes(query));

  if (filtered.length === 0) {
    previewList.innerHTML = '<div style="color:var(--text-3); font-size:12px;">无匹配的表名</div>';
    return;
  }

  previewList.innerHTML = filtered.map(name => `
    <div class="db-table-item">
      <div class="db-table-meta-name">
        <svg class="icon icon-sm db-table-icon" aria-hidden="true" style="color:var(--text-3);"><use href="#icon-server"/></svg>
        <span>${escHtml(name)}</span>
      </div>
      <span class="db-table-badge">TABLE</span>
    </div>
  `).join('');
}

function hideSchemaRow() {
  const row = document.getElementById('db-schema-row');
  if (row) row.style.display = 'none';
}

// ═══════════════════════════════════════════════════
//  Phase 4 — 诊断采集
// ═══════════════════════════════════════════════════

function initPhase4() {
  document.getElementById('mode-realtime').addEventListener('click',   () => switchMode('realtime'));
  document.getElementById('mode-history').addEventListener('click',    () => switchMode('history'));
  document.getElementById('mode-scheduler').addEventListener('click',  () => switchMode('scheduler'));
  document.getElementById('mode-quick').addEventListener('click',      () => switchMode('quick'));

  // Realtime
  document.getElementById('btn-open-browser').addEventListener('click', openDiagBrowser);
  document.getElementById('btn-capture-done').addEventListener('click', stopCapture);
  document.getElementById('btn-reset-capture').addEventListener('click', resetCapture);
  document.getElementById('btn-close-browser').addEventListener('click', closeDiagBrowser);
  document.getElementById('btn-diag-devtools').addEventListener('click', openDiagDevtools);
  document.getElementById('btn-new-diag').addEventListener('click', resetRealtime);

  // History
  document.getElementById('btn-history-run').addEventListener('click', runHistoricalDiagnosis);
  document.getElementById('btn-history-new').addEventListener('click', resetHistorySection);
  document.getElementById('btn-set-wide-range').addEventListener('click', setTodayRange);
  document.getElementById('history-keywords').addEventListener('input', detectTraceIdInput);

  // Scheduler
  document.getElementById('btn-backto-phase3-from-4').addEventListener('click', () => jumpToStep(3));
  document.getElementById('btn-start-sched').addEventListener('click', startScheduler);
  document.getElementById('btn-stop-sched').addEventListener('click', stopScheduler);
  document.getElementById('btn-sched-stop-bar').addEventListener('click', stopScheduler);

  // Quick
  document.getElementById('btn-quick-run').addEventListener('click', runQuickDiagnosis);
  document.getElementById('btn-quick-new').addEventListener('click', resetQuickSection);
}

async function preparePhase4() {
  const outputDir = document.getElementById('output-path').value.trim() || null;
  try {
    await invoke('confirm_validation', { outputDir });
    state.validationConfirmed = true;
  } catch (err) {
    console.warn('confirm_validation failed:', err);
    showToast('配置校验确认失败，请重试', 'error');
    state.validationConfirmed = false;
  }

  // 根据定时巡检开关状态展示状态条
  document.getElementById('sched-bar').style.display = state.scheduleEnabled ? 'flex' : 'none';
  startSchedulerPolling();
}

function switchMode(mode) {
  // SSH 日志源仅支持实时模式（历史/快速/定时巡检依赖 ELK/ES 的关键词与 traceId 检索）
  if (state.activeSource === 'ssh' && mode !== 'realtime') {
    showToast('SSH 日志源仅支持实时诊断，历史/快速/定时巡检请切换为 ELK 或 ES', 'info', 5000);
    mode = 'realtime';
  }
  state.diagMode = mode;
  ['realtime', 'history', 'scheduler', 'quick'].forEach(m => {
    const card = document.getElementById('mode-' + m);
    const sec = document.getElementById('section-' + m);
    if (card) card.classList.toggle('selected', m === mode);
    if (sec) sec.style.display = m === mode ? 'block' : 'none';
  });
}

// ─── Realtime mode ───

async function openDiagBrowser() {
  const url = document.getElementById('page-url').value.trim();
  if (!url) { showToast('请输入目标 URL', 'error'); return; }
  try { new URL(url); } catch {
    showToast('URL 格式不合法，请输入完整地址', 'error');
    return;
  }

  const btn = document.getElementById('btn-open-browser');
  btn.disabled = true;

  try {
    await invoke('open_diag_browser', { url });
    state.diagBrowserOpen = true;
    state.capturedRequests = [];

    document.getElementById('capture-card').style.display = 'block';
    document.getElementById('capture-count').textContent = '0';
    document.getElementById('req-tbody').innerHTML = '';
    document.getElementById('capture-empty').style.display = 'block';
    document.getElementById('capture-dot').className = 'status-dot testing';
    document.getElementById('capture-indicator').textContent = '诊断浏览器抓包中';
    document.getElementById('btn-capture-done').disabled = false;

    startCountPolling();
    showToast('诊断网页已打开，请重现您遇到的故障', 'info');
  } catch (err) {
    showToast('启动内嵌浏览器失败: ' + err, 'error');
  }
  btn.disabled = false;
}

function startCountPolling() {
  stopCountPolling();
  let lastCount = -1;
  state.countPollingTimer = setInterval(async () => {
    try {
      const count = await invoke('get_capture_count', {});
      if (count !== lastCount) {
        lastCount = count;
        document.getElementById('capture-count').textContent = count;
        if (count > 0) {
          document.getElementById('capture-empty').style.display = 'none';
        }
      }
    } catch { /* ignore polling errors */ }
  }, 500);
}

function stopCountPolling() {
  if (state.countPollingTimer) {
    clearInterval(state.countPollingTimer);
    state.countPollingTimer = null;
  }
}

async function closeDiagBrowser() {
  stopCountPolling();
  try { await invoke('close_diag_browser', {}); } catch { /* ignore */ }
  state.diagBrowserOpen = false;
  document.getElementById('capture-dot').className = 'status-dot idle';
  document.getElementById('capture-indicator').textContent = '抓包已停止';
}

async function openDiagDevtools() {
  try {
    await invoke('open_diag_devtools', {});
  } catch (err) {
    showToast('开发者工具打开失败: ' + err, 'error');
  }
}

async function resetCapture() {
  const btn = document.getElementById('btn-reset-capture');
  btn.disabled = true;
  btn.textContent = '重置中...';
  try {
    const targetUrl = document.getElementById('page-url').value.trim() || null;
    const msg = await invoke('reset_capture_data', { targetUrl });
    document.getElementById('req-tbody').innerHTML = '';
    document.getElementById('capture-count').textContent = '0';
    const empty = document.getElementById('capture-empty');
    if (empty) empty.style.display = 'flex';
    showToast(msg || '已重置采集数据', 'success');
  } catch (err) {
    showToast('重置失败: ' + err, 'error');
  } finally {
    btn.disabled = false;
    btn.innerHTML = '<svg class="icon icon-sm" aria-hidden="true"><use href="#icon-refresh"/></svg> 重置采集';
  }
}

async function stopCapture() {
  const btn = document.getElementById('btn-capture-done');
  btn.disabled = true;

  try {
    const capturedJson = await invoke('collect_diag_data', {});
    const capturedData = JSON.parse(capturedJson);

    if (!capturedData.requests || capturedData.requests.length === 0) {
      showToast('未捕获到请求，请在诊断浏览器内操作页面', 'error');
      btn.disabled = false;
      return;
    }

    state.capturedRequests = capturedData.requests;
    const gatewayPrefix = document.getElementById('gateway-prefix').value.trim() || '/gateway';

    const tbody = document.getElementById('req-tbody');
    tbody.innerHTML = '';
    document.getElementById('capture-empty').style.display = 'none';

    capturedData.requests.forEach(req => {
      const { svc, path } = parseRequestUrl(req.url, gatewayPrefix);
      appendRequestRow(tbody, req, path);
    });

    document.getElementById('capture-count').textContent = capturedData.requests.length;

    await closeDiagBrowser();
    stopCountPolling();
    await sleep(400);

    document.getElementById('capture-card').style.display = 'none';
    document.getElementById('progress-card').style.display = 'block';

    await runDiagnosis(capturedJson);
  } catch (err) {
    showToast('读取抓包数据失败: ' + err, 'error');
    btn.disabled = false;
  }
}

function parseRequestUrl(url, gatewayPrefix) {
  try {
    const urlObj = new URL(url);
    const pathname = urlObj.pathname;
    if (pathname.startsWith(gatewayPrefix + '/')) {
      const rest = pathname.slice(gatewayPrefix.length + 1);
      const slash = rest.indexOf('/');
      const svc  = slash > 0 ? rest.slice(0, slash) : rest;
      const path = slash > 0 ? rest.slice(slash) : '/';
      return { svc, path };
    }
    return { svc: 'unknown', path: pathname };
  } catch {
    return { svc: 'unknown', path: url };
  }
}

function appendRequestRow(tbody, req, path) {
  const dur = req.durationMs;
  const timeStr = dur >= 1000 ? (dur / 1000).toFixed(2) + ' s' : dur + ' ms';
  const timeClass = dur > 2000 ? 't-slow' : dur > 1000 ? 't-warn' : '';
  const sizeStr = req.responseSize ? formatBytes(req.responseSize) : '-';
  const displayPath = path.length > 50 ? '...' + path.slice(-47) : path;

  // HTTP Method and Status styling
  const method = (req.method || 'GET').toLowerCase();
  const methodClass = method === 'get' ? 'get' : method === 'post' ? 'post' : 'xhr';
  const status = req.status || 0;
  const statusClass = status >= 500 ? 'err' : status >= 400 ? 'warn' : 'ok';

  const tr = document.createElement('tr');
  tr.innerHTML = `
    <td class="t-name" title="${escHtml(req.url)}">
      <span class="badge-method ${methodClass}">${method.toUpperCase()}</span>${escHtml(displayPath)}
    </td>
    <td><span class="status-pill ${statusClass}">${status || '-'}</span></td>
    <td>xhr</td>
    <td>${sizeStr}</td>
    <td class="${timeClass}">${timeStr}</td>
  `;
  tbody.appendChild(tr);
}

async function runDiagnosis(capturedJson) {
  const activeSource = requireActiveSource();
  if (!activeSource) return;

  const stepDefs = [
    '解析请求 URL，匹配后端服务',
    'SSH/ELK 采集链路关联日志',
    '从日志提取并匹配慢 SQL',
    '运行隐私敏感数据脱敏',
    '压缩并打包 diagnosis.zip',
  ];

  const stepsEl = document.getElementById('progress-steps');
  stepsEl.innerHTML = stepDefs.map((s, i) =>
    `<div class="p-step" id="pstep-${i}">
      <span class="p-step-icon"></span>
      <span>${s}</span>
    </div>`
  ).join('');

  let result = null;
  let diagErr = null;

  for (let i = 0; i < stepDefs.length; i++) {
    const stepEl = document.getElementById(`pstep-${i}`);
    stepEl.classList.add('active');
    stepEl.querySelector('.p-step-icon').innerHTML =
      `<span class="spinner" style="width:12px;height:12px"></span>`;
    setProgressBar('progress-bar', (i + 0.5) / stepDefs.length);

    if (i === stepDefs.length - 1) {
      try {
        result = await invoke('start_diagnosis', { capturedJson, logSource: activeSource });
      } catch (err) {
        diagErr = err;
      }
    } else {
      await sleep(600);
    }

    stepEl.classList.remove('active');
    stepEl.classList.add('done');
    stepEl.querySelector('.p-step-icon').innerHTML =
      `<svg class="icon icon-sm" style="color:var(--success)" aria-hidden="true"><use href="#icon-check"/></svg>`;
    setProgressBar('progress-bar', (i + 1) / stepDefs.length);
  }

  if (diagErr) {
    showMsg('diag-msg', '诊断执行失败：' + diagErr, 'error');
  } else if (result) {
    showResultCard(result);
  }
}

function setProgressBar(barId, fraction) {
  const bar = document.getElementById(barId);
  if (bar) bar.style.width = Math.min(fraction * 100, 100) + '%';
}

function showResultCard(result) {
  document.getElementById('progress-card').style.display = 'none';
  const card = document.getElementById('result-card');
  card.style.display = 'block';
  document.getElementById('result-path').textContent = result.outputPath || '-';

  const container = document.getElementById('result-services');
  const services = result.services || [];

  if (services.length === 0) {
    container.innerHTML = '<div class="empty-state"><p>未匹配到关联的服务数据</p></div>';
    return;
  }

  container.innerHTML = services.map(svc => `
    <div class="svc-row">
      <span class="svc-row-name">${escHtml(svc.name || svc || '-')}</span>
      <span class="svc-row-meta">
        <span>${svc.requestCount ?? 0} 请求</span>
        ${svc.errorCount  > 0 ? `<span class="m-err">${svc.errorCount} 错误</span>` : ''}
        ${svc.logs?.length > 0 ? `<span>${svc.logs.length} 日志</span>` : ''}
        ${svc.slowSqls?.length > 0 ? `<span class="m-warn">${svc.slowSqls.length} 慢SQL</span>` : ''}
      </span>
    </div>
  `).join('');
}

function resetRealtime() {
  closeDiagBrowser();
  document.getElementById('capture-card').style.display = 'none';
  document.getElementById('progress-card').style.display = 'none';
  document.getElementById('result-card').style.display = 'none';
  document.getElementById('page-url').value = '';
  document.getElementById('req-tbody').innerHTML = '';
  document.getElementById('capture-count').textContent = '0';
  document.getElementById('btn-capture-done').disabled = false;
  setProgressBar('progress-bar', 0);
}

// ─── History mode ───

function setDefaultTimeRange() {
  const end   = new Date();
  const start = new Date(end - 3600 * 1000);
  const fmt = d => d.toISOString().slice(0, 16);
  const startEl = document.getElementById('history-start');
  const endEl   = document.getElementById('history-end');
  if (startEl) startEl.value = fmt(start);
  if (endEl)   endEl.value   = fmt(end);
}

function setTodayRange() {
  const now   = new Date();
  const start = new Date(now.getFullYear(), now.getMonth(), now.getDate(), 0, 0);
  const end   = new Date(now.getFullYear(), now.getMonth(), now.getDate(), 23, 59);
  const fmt = d => d.toISOString().slice(0, 16);
  const startEl = document.getElementById('history-start');
  const endEl   = document.getElementById('history-end');
  if (startEl) startEl.value = fmt(start);
  if (endEl)   endEl.value   = fmt(end);
  showToast('时间范围已扩展为今日 00:00 ~ 23:59', 'success');
}

function detectTraceIdInput() {
  const val = document.getElementById('history-keywords').value.trim();
  const badge = document.getElementById('traceid-badge');
  if (!badge) return;
  const hasTraceId = val.split(/\s+/).some(k =>
    k.includes('.') && /^[0-9a-fA-F.]+$/.test(k)
  );
  badge.style.display = hasTraceId ? 'inline-block' : 'none';
}

function toISOWithSeconds(val) {
  if (!val) return null;
  return val + ':00+00:00';
}

const HIST_STEPS = [
  { id: 'elk-connect',    label: '连接 ELK 服务',      weight: 1 },
  { id: 'elk-query',      label: '查询时间段内日志',    weight: 2 },
  { id: 'extract-trace',  label: '提取 traceId 链路',  weight: 1 },
  { id: 'collect-logs',   label: '拉取全部服务日志',    weight: 3 },
  { id: 'query-sql',      label: '慢 SQL 抓取',        weight: 1 },
  { id: 'masking',        label: '脱敏处理',            weight: 1 },
  { id: 'package',        label: '生成 diagnosis.zip', weight: 1 },
];
const HIST_TOTAL_WEIGHT = HIST_STEPS.reduce((s, x) => s + x.weight, 0);

async function runHistoricalDiagnosis() {
  const activeSource = requireActiveSource();
  if (!activeSource) return;

  const keywordsRaw = document.getElementById('history-keywords').value.trim();
  if (!keywordsRaw) { showToast('请输入查询关键词/traceId', 'error'); return; }

  const startVal = document.getElementById('history-start').value;
  const endVal   = document.getElementById('history-end').value;
  if (!startVal || !endVal) { showToast('请选择时间范围', 'error'); return; }
  if (new Date(startVal) >= new Date(endVal)) {
    showToast('结束时间需晚于开始时间', 'error');
    return;
  }

  const keywords  = keywordsRaw.split(/\s+/).filter(Boolean);
  const timeStart = toISOWithSeconds(startVal);
  const timeEnd   = toISOWithSeconds(endVal);

  const btn = document.getElementById('btn-history-run');
  btn.disabled = true;

  document.getElementById('history-result-card').style.display = 'none';
  document.getElementById('history-progress-card').style.display = 'block';

  const stepsEl = document.getElementById('history-progress-steps');
  stepsEl.innerHTML = HIST_STEPS.map(s =>
    `<div class="p-step" id="hpstep-${s.id}">
      <span class="p-step-icon p-step-pending"></span>
      <div class="p-step-body">
        <span class="p-step-label">${s.label}</span>
        <span class="p-step-detail" id="hpdetail-${s.id}"></span>
      </div>
    </div>`
  ).join('');

  setProgressBar('history-progress-bar', 0);
  let doneWeight = 0;

  let unlisten = null;
  if (window.__TAURI__?.event?.listen) {
    unlisten = await window.__TAURI__.event.listen('history-step', ev => {
      const { step, detail, status } = ev.payload || {};
      const stepDef = HIST_STEPS.find(s => s.id === step);
      if (!stepDef) return;

      const el     = document.getElementById(`hpstep-${step}`);
      const detEl  = document.getElementById(`hpdetail-${step}`);
      const iconEl = el?.querySelector('.p-step-icon');
      if (!el) return;

      el.classList.remove('active', 'done', 'error');

      if (status === 'running') {
        el.classList.add('active');
        if (iconEl) iconEl.innerHTML = `<span class="spinner" style="width:11px;height:11px;border-width:1.5px"></span>`;
        if (detEl) detEl.textContent = detail || '';
      } else if (status === 'done') {
        el.classList.add('done');
        if (iconEl) iconEl.innerHTML = `<svg class="icon icon-sm" style="color:var(--success)" aria-hidden="true"><use href="#icon-check"/></svg>`;
        if (detEl) detEl.textContent = detail || '完成';
        doneWeight += stepDef.weight;
        setProgressBar('history-progress-bar', doneWeight / HIST_TOTAL_WEIGHT);
      } else if (status === 'error') {
        el.classList.add('error');
        if (iconEl) iconEl.innerHTML = `<svg class="icon icon-sm" style="color:var(--error)" aria-hidden="true"><use href="#icon-x"/></svg>`;
        if (detEl) { detEl.textContent = detail || '错误'; detEl.style.color = 'var(--error)'; }
      } else if (status === 'skip') {
        if (iconEl) iconEl.innerHTML = `<svg class="icon icon-sm" style="color:var(--text-3)" aria-hidden="true"><use href="#icon-info"/></svg>`;
        if (detEl) detEl.textContent = detail || '已跳过';
        doneWeight += stepDef.weight;
        setProgressBar('history-progress-bar', doneWeight / HIST_TOTAL_WEIGHT);
      }
    });
  }

  try {
    const result = await invoke('start_historical_diagnosis', { keywords, timeStart, timeEnd, logSource: activeSource });
    setProgressBar('history-progress-bar', 1);
    showHistoryResult(result);
  } catch (err) {
    showMsg('history-diag-msg', String(err), 'error');
    showToast(String(err), 'error', 6000);
  } finally {
    if (unlisten) unlisten();
    btn.disabled = false;
  }
}

function showHistoryResult(result) {
  const card = document.getElementById('history-result-card');
  card.style.display = 'block';
  document.getElementById('history-result-path').textContent = result.outputPath || '-';

  const logCount   = result.logCount   ?? 0;
  const traceCount = result.traceCount ?? 0;
  const errorCount = result.errorCount ?? 0;
  const warnCount  = result.warnCount  ?? 0;
  const noTrace    = result.noTraceCount ?? 0;

  document.getElementById('history-result-info').innerHTML = `
    <div style="display:flex;flex-wrap:wrap;gap:16px;font-size:12px;margin-top:8px">
      <span>采集日志数量: <strong style="color:var(--text-1)">${logCount}</strong> 条 (错误 ${errorCount} · 警告 ${warnCount})</span>
      <span>提取链路 traceId: <strong style="color:var(--accent-txt)">${traceCount}</strong> 个 ${noTrace > 0 ? `(${noTrace}条无 ID)` : ''}</span>
    </div>
  `;
}

function resetHistorySection() {
  document.getElementById('history-keywords').value = '';
  setDefaultTimeRange();
  document.getElementById('history-progress-card').style.display = 'none';
  document.getElementById('history-result-card').style.display = 'none';
  document.getElementById('history-diag-msg').innerHTML = '';
  setProgressBar('history-progress-bar', 0);
}

// ─── Scheduler mode ───

async function startScheduler() {
  try {
    await invoke('start_scheduler', {});
    showToast('定时巡检服务已启动', 'success');
    document.getElementById('btn-start-sched').disabled = true;
    document.getElementById('btn-stop-sched').disabled  = false;
    updateSchedulerDot(true);
    document.getElementById('sched-badge').className = 'sched-badge running';
    document.getElementById('sched-badge').textContent = '运行中';
  } catch (err) {
    showToast('定时巡检启动失败: ' + err, 'error');
  }
}

async function stopScheduler() {
  try {
    await invoke('stop_scheduler', {});
    showToast('定时巡检服务已停止', 'info');
    document.getElementById('btn-start-sched').disabled = false;
    document.getElementById('btn-stop-sched').disabled  = true;
    updateSchedulerDot(false);
    document.getElementById('sched-badge').className = 'sched-badge';
    document.getElementById('sched-badge').textContent = '已停止';
  } catch (err) {
    showToast('停止定时巡检失败: ' + err, 'error');
  }
}

function updateSchedulerDot(running) {
  const dot = document.getElementById('scheduler-dot');
  const txt = document.getElementById('scheduler-status-txt');
  if (!dot) return;
  dot.className = `status-dot ${running ? 'ok' : 'idle'}`;
  if (txt) {
    txt.textContent = running ? '运行中' : '已停止';
    txt.style.color = running ? 'var(--success)' : 'var(--text-3)';
  }
}

function startSchedulerPolling() {
  stopSchedulerPolling();
  pollSchedulerStatus();
  state.schedulerPollingTimer = setInterval(pollSchedulerStatus, 10000);
}

function stopSchedulerPolling() {
  if (state.schedulerPollingTimer) {
    clearInterval(state.schedulerPollingTimer);
    state.schedulerPollingTimer = null;
  }
}

async function pollSchedulerStatus() {
  try {
    const status = await invoke('get_scheduler_status', {});
    const running = status.running || false;

    updateSchedulerDot(running);

    const badge = document.getElementById('sched-badge');
    badge.className = 'sched-badge' + (running ? ' running' : '');
    badge.textContent = running ? '运行中' : '已停止';

    const lastRunEl = document.getElementById('sched-last-run');
    if (status.lastRunAt && lastRunEl) {
      lastRunEl.textContent = '上次运行: ' + formatDateTime(status.lastRunAt);
    }

    const pkgEl = document.getElementById('sched-pkg-count');
    if (status.packagesCreated != null && pkgEl) {
      pkgEl.textContent = '已打包数: ' + status.packagesCreated;
    }

    document.getElementById('btn-start-sched').disabled = running;
    document.getElementById('btn-stop-sched').disabled  = !running;

    if (status.recentPackages && status.recentPackages.length > 0) {
      renderSchedulerPackages(status.recentPackages);
    }
  } catch { /* ignore polling errors */ }
}

function renderSchedulerPackages(packages) {
  const listEl = document.getElementById('sched-pkg-list');
  const emptyEl = document.getElementById('sched-empty');
  if (emptyEl) emptyEl.style.display = 'none';

  const existingItems = listEl.querySelectorAll('.pkg-item');
  existingItems.forEach(el => el.remove());

  packages.forEach(pkg => {
    const item = document.createElement('div');
    item.className = 'pkg-item';
    item.innerHTML = `
      <span class="pkg-item-name" style="cursor:pointer;" title="点击打开目录" data-path="${escHtml(pkg.path || '')}">${escHtml(pkg.fileName || (pkg.path || '').split('/').pop() || '-')}</span>
      <span class="pkg-item-meta">${pkg.traceCount ?? '?'} traces · ${formatDateTime(pkg.createdAt)}</span>
    `;
    item.querySelector('.pkg-item-name').addEventListener('click', () => openFolder(pkg.path));
    listEl.appendChild(item);
  });
}

function refreshSchedulerPackageList(payload) {
  const listEl = document.getElementById('sched-pkg-list');
  if (!listEl) return;
  const emptyEl = document.getElementById('sched-empty');
  if (emptyEl) emptyEl.style.display = 'none';

  const item = document.createElement('div');
  item.className = 'pkg-item';
  const displayPath = payload.outputPath || '';
  const displayName = payload.fileName || displayPath.split('/').pop() || '新诊断包';
  item.innerHTML = `
    <span class="pkg-item-name" style="cursor:pointer;" title="点击打开目录">${escHtml(displayName)}</span>
    <span class="pkg-item-meta">${payload.traceCount ?? '?'} traces · 刚刚</span>
  `;
  item.querySelector('.pkg-item-name').addEventListener('click', () => openFolder(displayPath));
  listEl.prepend(item);
}

// ─── Quick Mode ───

async function runQuickDiagnosis() {
  const activeSource = requireActiveSource();
  if (!activeSource) return;

  const traceId = document.getElementById('quick-trace-id').value.trim();
  if (!traceId) { showToast('请输入 traceId', 'error'); return; }

  const btn = document.getElementById('btn-quick-run');
  btn.disabled = true;

  document.getElementById('quick-progress-card').style.display = 'block';
  document.getElementById('quick-result-card').style.display = 'none';
  document.getElementById('quick-progress-steps').innerHTML = '';

  let unlisten = null;
  if (window.__TAURI__?.event?.listen) {
    unlisten = await window.__TAURI__.event.listen('quick-diag-step', (ev) => {
      const { step, detail, status } = ev.payload || {};
      updateQuickStep(step, detail, status);
    });
  }

  try {
    const result = await invoke('start_quick_diagnosis', {
      traceId,
      fieldTraceId: null,
      fieldMessage: null,
      indexPattern: null,
      logSource: activeSource,
    });

    document.getElementById('quick-result-card').style.display = 'block';
    document.getElementById('quick-result-path').textContent = result.outputPath || '-';
    document.getElementById('quick-result-info').innerHTML =
      `关联日志: ${result.logCount} 条 · SQL 指令: ${result.sqlCount} 条 · 包含 EXPLAIN 评估: ${result.explainCount} 个`;
    showToast('快速诊断包生成成功', 'success');
  } catch (e) {
    updateQuickStep('error', String(e), 'error');
    showToast('快速诊断执行失败: ' + e, 'error', 6000);
  } finally {
    btn.disabled = false;
    if (unlisten) unlisten();
  }
}

function updateQuickStep(step, detail, status) {
  const container = document.getElementById('quick-progress-steps');
  let el = container.querySelector(`[data-step="${step}"]`);
  if (!el) {
    el = document.createElement('div');
    el.className = 'step-line';
    el.dataset.step = step;
    container.appendChild(el);
  }

  const icon = status === 'done' ? '✓' : status === 'error' ? '✗' : status === 'skip' ? '—' : '⟳';
  const color = status === 'done' ? 'var(--success)'
    : status === 'error' ? 'var(--error)'
    : status === 'skip' ? 'var(--text-3)'
    : 'var(--accent-txt)';

  el.innerHTML = `<span style="color:${color};font-weight:600;margin-right:6px">${icon}</span>${escHtml(detail)}`;
  el.style.fontSize = '12px';
  el.style.lineHeight = '1.8';
  el.style.color = 'var(--text-2)';
}

function resetQuickSection() {
  document.getElementById('quick-progress-card').style.display = 'none';
  document.getElementById('quick-result-card').style.display = 'none';
  document.getElementById('quick-progress-steps').innerHTML = '';
  document.getElementById('quick-trace-id').value = '';
}

// ═══════════════════════════════════════════════════
//  Config Persistence (保存/加载)
// ═══════════════════════════════════════════════════

async function tryLoadSavedConfig() {
  try {
    const manifest = await invoke('load_config_from_disk', {});
    if (!manifest) return false;
    fillFormFromManifest(manifest);
    showConfigStatus(true, '已加载历史配置');
    return true;
  } catch {
    return false;
  }
}

function fillFormFromManifest(m) {
  if (m.siteName)       setVal('site-name', m.siteName);
  if (m.gatewayPrefix)  setVal('gateway-prefix', m.gatewayPrefix);

  if (m.elk) {
    const e = m.elk;
    setVal('elk-address',   e.address || '');
    setVal('elk-index-pattern', e.indexPattern || 'logstash-*');
    setVal('elk-username',  e.username || '');
    setVal('elk-password',  e.password || '');
    if (e.fieldTimestamp) setVal('elk-field-timestamp', e.fieldTimestamp);
    if (e.fieldLevel)     setVal('elk-field-level',     e.fieldLevel);
    if (e.fieldTraceId)   setVal('elk-field-traceid',   e.fieldTraceId);
    if (e.fieldService)   setVal('elk-field-service',   e.fieldService);
    if (e.fieldMessage)   setVal('elk-field-message',   e.fieldMessage);
    state.elkAddressFilled = !!(e.address);
  }

  if (m.es) {
    const e = m.es;
    setVal('es-address',   e.address || '');
    setVal('es-index-pattern', e.indexPattern || 'logstash-*');
    setVal('es-username',  e.username || '');
    setVal('es-password',  e.password || '');
    if (e.fieldTimestamp) setVal('es-field-timestamp', e.fieldTimestamp);
    if (e.fieldLevel)     setVal('es-field-level',     e.fieldLevel);
    if (e.fieldTraceId)   setVal('es-field-traceid',   e.fieldTraceId);
    if (e.fieldService)   setVal('es-field-service',   e.fieldService);
    if (e.fieldMessage)   setVal('es-field-message',   e.fieldMessage);
    state.esAddressFilled = !!(e.address);
  }

  // 先恢复 SSH 服务列表 UI——无论当前激活哪种日志源，只要存在服务配置就回填预览，
  // 否则切到 SSH 源后 svcImported 仍为 false 会卡在第二步无法继续。
  if (m.services && m.services.length > 0) {
    const firstIp = m.services[0].serverIp;
    if (firstIp && firstIp !== 'elk' && firstIp !== 'es') {
      state.svcImported = true;
      const drop = document.getElementById('svc-drop');
      drop.classList.add('loaded');
      drop.querySelector('.file-drop-text').textContent = `已成功从磁盘加载 ${m.services.length} 个服务配置`;
      renderSvcPreview(m.services);
    }
  }

  // 恢复活跃的 logSource——优先从 manifest.logSource 读取，其次按配置内容推断（兼容旧数据）
  const savedSource = m.logSource;
  if (savedSource === 'es' || savedSource === 'elk' || savedSource === 'ssh') {
    setLogSource(savedSource);
  } else if (m.es && (!m.elk || !m.elk.address)) {
    setLogSource('es');
  } else if (m.elk) {
    setLogSource('elk');
  } else if (state.svcImported) {
    setLogSource('ssh');
  }

  if (m.databases && m.databases.length > 0) {
    const db = m.databases[0];
    const dbType = db.dbType || 'mysql';
    setDbType(dbType);
    setVal('db-host',     db.host || '');
    setVal('db-port',     String(db.port || (dbType === 'postgresql' ? '5432' : '3306')));
    setVal('db-username', db.username || '');
    setVal('db-password', db.password || '');
    setVal('db-name',     db.database || '');
  }

  if (m.schedule) {
    const s = m.schedule;
    const schedCheck = document.getElementById('schedule-enabled');
    if (schedCheck) {
      schedCheck.checked = !!s.enabled;
      state.scheduleEnabled = !!s.enabled;
      document.getElementById('schedule-fields').style.display = s.enabled ? 'block' : 'none';
    }
    if (s.intervalMinutes) setVal('schedule-interval',  String(s.intervalMinutes));
    if (s.lookbackMinutes) setVal('schedule-lookback',  String(s.lookbackMinutes));
    if (s.maxTraceIdsPerRun) setVal('schedule-max-traces', String(s.maxTraceIdsPerRun));
    if (s.dedupWindowMinutes) setVal('schedule-dedup-window', String(s.dedupWindowMinutes));
    if (s.extraKeywords && s.extraKeywords.length)
      setVal('schedule-extra-keywords', s.extraKeywords.join(' '));
    if (s.outputRetentionDays) setVal('output-retention', String(s.outputRetentionDays));
  }

  checkPhase1Ready();
}

async function autoSaveConfig() {
  try {
    await invoke('save_config_to_disk', {});
    showConfigStatus(true, '配置已保存');
  } catch (err) {
    showConfigStatus(false, '保存配置失败');
  }
}

function showConfigStatus(ok, text) {
  const statusEl = document.getElementById('config-status');
  const iconEl   = document.getElementById('config-status-icon');
  const textEl   = document.getElementById('config-status-text');
  const clearBtn = document.getElementById('btn-clear-config');

  if (statusEl) {
    statusEl.style.display = 'flex';
    if (iconEl) iconEl.querySelector('use').setAttribute('href', ok ? '#icon-check' : '#icon-warning');
    if (textEl) textEl.textContent = text;
    statusEl.style.color = ok ? 'var(--success)' : 'var(--warn)';
  }
  if (clearBtn) clearBtn.style.display = ok ? 'flex' : 'none';
}

async function clearAllConfig() {
  const confirmed = await confirmDialog('确认要清空所有已保存配置吗？');
  if (!confirmed) return;

  const btn = document.getElementById('btn-clear-config');
  btn.disabled = true;
  try {
    await invoke('clear_saved_config', {});
    ['site-name','gateway-prefix','elk-address','elk-index-pattern','elk-username','elk-password',
     'db-host','db-port','db-username','db-password','db-name',
     'schedule-interval','schedule-lookback','schedule-max-traces','schedule-dedup-window','schedule-extra-keywords'].forEach(id => {
      const el = document.getElementById(id);
      if (el) el.value = '';
    });
    setVal('elk-index-pattern', 'logstash-*');
    setDbType('mysql');
    setLogSource('elk');
    state.elkAddressFilled = false;
    state.scheduleEnabled  = false;
    document.getElementById('schedule-fields').style.display = 'none';
    const schedCheck = document.getElementById('schedule-enabled');
    if (schedCheck) schedCheck.checked = false;

    document.getElementById('config-status').style.display = 'none';
    document.getElementById('btn-clear-config').style.display = 'none';

    jumpToStep(1);
    await prefillOutputPath();
    showToast('配置已清空', 'success');
  } catch (err) {
    showToast('清空失败: ' + err, 'error');
  } finally {
    btn.disabled = false;
  }
}

// ═══════════════════════════════════════════════════
//  Utilities
// ═══════════════════════════════════════════════════

function setupResultActionButtons() {
  // Realtime mode
  document.getElementById('btn-open-dir').addEventListener('click', () => {
    openFolder(document.getElementById('result-path').textContent);
  });
  document.getElementById('btn-copy-path').addEventListener('click', () => {
    copyToClipboard(document.getElementById('result-path').textContent);
  });

  // History mode
  document.getElementById('btn-open-dir-hist').addEventListener('click', () => {
    openFolder(document.getElementById('history-result-path').textContent);
  });
  document.getElementById('btn-copy-path-hist').addEventListener('click', () => {
    copyToClipboard(document.getElementById('history-result-path').textContent);
  });

  // Quick mode
  document.getElementById('btn-open-dir-quick').addEventListener('click', () => {
    openFolder(document.getElementById('quick-result-path').textContent);
  });
  document.getElementById('btn-copy-path-quick').addEventListener('click', () => {
    copyToClipboard(document.getElementById('quick-result-path').textContent);
  });
}

async function openFolder(path) {
  if (!path || path === '-') return;
  try {
    await invoke('open_output_dir', { path });
    showToast('已打开输出文件夹', 'success');
  } catch (err) {
    showToast('打开文件夹失败: ' + err, 'error');
  }
}

function copyToClipboard(text) {
  if (!text || text === '-') return;
  navigator.clipboard.writeText(text)
    .then(() => showToast('路径已成功复制到剪贴板', 'success'))
    .catch(err => showToast('复制失败: ' + err, 'error'));
}

async function confirmDialog(msg) {
  try {
    if (window.__TAURI__?.dialog) {
      const { ask } = window.__TAURI__.dialog;
      return await ask(msg, { title: '确认提示', kind: 'warning', okLabel: '确认', cancelLabel: '取消' });
    }
  } catch { /* fallback */ }
  return window.confirm(msg);
}

async function prefillOutputPath() {
  try {
    const desktopPath = await invoke('get_desktop_path', {});
    const pathInput = document.getElementById('output-path');
    if (desktopPath && !pathInput.value) {
      const sep = desktopPath.includes('\\') ? '\\' : '/';
      pathInput.value = desktopPath + sep + 'diagnosis-output';
    }
  } catch { /* ignore */ }
}

async function browseOutputPath() {
  try {
    const selected = await invoke('pick_output_folder', {});
    if (selected) {
      document.getElementById('output-path').value = selected;
    }
  } catch (err) {
    showToast('浏览文件夹失败，请手动配置路径', 'error');
  }
}

async function downloadTemplate() {
  try {
    const result = await invoke('export_template', {});
    showToast(`部署配置模板已生成，请查看：${result.serviceTemplate}`, 'success', 5000);
  } catch (err) {
    showToast('生成配置模板失败: ' + err, 'error');
  }
}

async function handleSvcFile(file) {
  const text = await file.text();
  const content = text.replace(/^﻿/, '');
  try {
    const result = await invoke('import_service_csv', { csvContent: content });
    state.svcImported = true;
    const drop = document.getElementById('svc-drop');
    drop.classList.add('loaded');
    drop.querySelector('.file-drop-text').textContent =
      `${file.name} — 已成功解析 ${result.serviceCount ?? '?'} 个服务`;
    renderSvcPreview(result.services || []);
    showMsg('svc-msg', `成功导入 ${result.serviceCount ?? '?'} 个服务部署参数`, 'success');
    checkPhase2Ready();
  } catch (err) {
    const rows = parseServiceCSV(content);
    if (rows.length > 0) {
      state.svcImported = true;
      const drop = document.getElementById('svc-drop');
      drop.classList.add('loaded');
      drop.querySelector('.file-drop-text').textContent = `${file.name} — 已成功解析 ${rows.length} 个服务`;
      renderSvcPreview(rows);
      showMsg('svc-msg', `本地兜底导入 ${rows.length} 个服务配置`, 'success');
      checkPhase2Ready();
    } else {
      showMsg('svc-msg', '服务配置文件格式有误，解析失败: ' + err, 'error');
    }
  }
}

function renderSvcPreview(services) {
  const el = document.getElementById('svc-preview');
  el.style.display = 'block';
  el.innerHTML = `<table>
    <thead><tr><th>服务名</th><th>IP 地址</th><th>SSH 用户</th><th>日志路径</th></tr></thead>
    <tbody>${services.map(s =>
      `<tr>
        <td>${s.projectName || '-'}</td>
        <td class="ip">${s.serverIp || s.server_ip || '-'}</td>
        <td>${s.sshUser || s.sshUsername || s.ssh_user || '-'}</td>
        <td>${s.logPath || s.log_path || '-'}</td>
      </tr>`
    ).join('')}</tbody>
  </table>`;
}

function parseServiceCSV(content) {
  const lines = content.split('\n').filter(l => l.trim() && !l.startsWith('#'));
  if (lines.length < 2) return [];
  return lines.slice(1).map(line => {
    const f = line.split(',').map(s => s.trim());
    if (f.length >= 5) {
      return { projectName: f[0], serverIp: f[1], sshUser: f[2], logPath: f[4] || '' };
    }
    return null;
  }).filter(Boolean);
}

function setupPasswordEye(inputId, btnId, iconId) {
  const btn = document.getElementById(btnId);
  const input = document.getElementById(inputId);
  const iconUse = document.getElementById(iconId);
  if (!btn || !input) return;
  btn.addEventListener('click', () => {
    const shown = input.type === 'text';
    input.type = shown ? 'password' : 'text';
    if (iconUse) iconUse.setAttribute('href', shown ? '#icon-eye' : '#icon-eye-off');
  });
}

function toggleAdvanced(toggleId, contentId) {
  const toggle = document.getElementById(toggleId);
  const content = document.getElementById(contentId);
  const open = content.classList.contains('open');
  content.classList.toggle('open', !open);
  toggle.classList.toggle('open', !open);
  toggle.setAttribute('aria-expanded', String(!open));
}

function buildElkConfig() {
  return {
    address:       document.getElementById('elk-address').value.trim(),
    indexPattern:  document.getElementById('elk-index-pattern').value.trim() || 'logstash-*',
    username:      document.getElementById('elk-username').value.trim() || null,
    password:      document.getElementById('elk-password').value || null,
    fieldTimestamp:document.getElementById('elk-field-timestamp').value.trim() || '@timestamp',
    fieldLevel:    document.getElementById('elk-field-level').value.trim() || 'level',
    fieldTraceId:  document.getElementById('elk-field-traceid').value.trim() || 'traceId',
    fieldService:  document.getElementById('elk-field-service').value.trim() || 'serviceName',
    fieldMessage:  document.getElementById('elk-field-message').value.trim() || 'message',
  };
}

function buildEsConfig() {
  return {
    address:       document.getElementById('es-address').value.trim(),
    indexPattern:  document.getElementById('es-index-pattern').value.trim() || 'logstash-*',
    username:      document.getElementById('es-username').value.trim() || null,
    password:      document.getElementById('es-password').value || null,
    fieldTimestamp:document.getElementById('es-field-timestamp').value.trim() || '@timestamp',
    fieldLevel:    document.getElementById('es-field-level').value.trim() || 'level',
    fieldTraceId:  document.getElementById('es-field-traceid').value.trim() || 'traceId',
    fieldService:  document.getElementById('es-field-service').value.trim() || 'serviceName',
    fieldMessage:  document.getElementById('es-field-message').value.trim() || 'message',
  };
}

function buildDbCsv() {
  const type = state.dbType;
  const host = document.getElementById('db-host').value.trim();
  const port = document.getElementById('db-port').value.trim();
  const user = document.getElementById('db-username').value.trim();
  const pass = document.getElementById('db-password').value;
  const name = document.getElementById('db-name').value.trim();
  return `数据库类型,服务器IP,端口,用户名,密码,数据库名\n${type},${host},${port},${user},${pass},${name}`;
}

function buildScheduleConfig() {
  const levels = [];
  if (document.getElementById('schedule-level-error').checked) levels.push('ERROR');
  if (document.getElementById('schedule-level-warn').checked)  levels.push('WARN');
  if (document.getElementById('schedule-level-info').checked)  levels.push('INFO');
  const extra = document.getElementById('schedule-extra-keywords').value.trim()
    .split(/\s+/).filter(Boolean);
  return {
    enabled:            document.getElementById('schedule-enabled').checked,
    intervalMinutes:    parseInt(document.getElementById('schedule-interval').value, 10) || 5,
    lookbackMinutes:    parseInt(document.getElementById('schedule-lookback').value, 10) || 6,
    levels,
    extraKeywords:      extra,
    maxTraceIdsPerRun:  parseInt(document.getElementById('schedule-max-traces').value, 10) || 50,
    dedupWindowMinutes: parseInt(document.getElementById('schedule-dedup-window').value, 10) || 60,
    outputRetentionDays:parseInt(document.getElementById('output-retention').value, 10) || 7,
  };
}

function setVerifyStatus(which, dotState, text, resultText, resultClass) {
  const dot     = document.getElementById(`${which}-status-dot`);
  const textEl  = document.getElementById(`${which}-status-text`);
  const resultEl= document.getElementById(`${which}-verify-result`);
  if (!dot) return;

  dot.className = `status-dot ${dotState}`;
  if (textEl) {
    textEl.textContent = text;
    textEl.style.color = dotState === 'ok'   ? 'var(--success)'
                       : dotState === 'fail'  ? 'var(--error)'
                       : dotState === 'testing'? 'var(--warn)'
                       : 'var(--text-3)';
  }
  if (resultEl && resultText !== undefined) {
    resultEl.textContent = resultText;
    resultEl.className = 'verify-result ' + (resultClass || '');
  }
}

function setVal(id, val) {
  const el = document.getElementById(id);
  if (el) el.value = val;
}

function formatBytes(bytes) {
  if (!bytes) return '-';
  if (bytes < 1024) return bytes + ' B';
  return (bytes / 1024).toFixed(1) + ' kB';
}

function formatDateTime(isoStr) {
  if (!isoStr) return '';
  try {
    const d = new Date(isoStr);
    return d.toLocaleString('zh-CN', {
      month: '2-digit', day: '2-digit',
      hour: '2-digit', minute: '2-digit',
    });
  } catch {
    return String(isoStr);
  }
}

function escHtml(str) {
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

function showMsg(containerId, text, type) {
  const el = document.getElementById(containerId);
  if (!el) return;
  el.style.display = 'block';
  el.className = `inline-msg ${type}`;
  el.innerHTML = `
    <svg class="icon icon-sm" aria-hidden="true"><use href="#icon-${type === 'error' ? 'x' : type === 'success' ? 'check' : 'info'}"/></svg>
    <span>${escHtml(text)}</span>
  `;
}

function showToast(msg, type = 'info', duration = 3000) {
  const container = document.getElementById('toast-container');
  if (!container) return;
  const toast = document.createElement('div');
  toast.className = `toast ${type}`;
  const iconRef = type === 'error' ? '#icon-x'
                : type === 'success' ? '#icon-check'
                : '#icon-info';
  toast.innerHTML = `
    <svg class="icon icon-sm" aria-hidden="true"><use href="${iconRef}"/></svg>
    <span>${escHtml(String(msg))}</span>
  `;
  container.appendChild(toast);
  requestAnimationFrame(() => requestAnimationFrame(() => toast.classList.add('show')));
  setTimeout(() => {
    toast.classList.remove('show');
    setTimeout(() => toast.remove(), 250);
  }, duration);
}
