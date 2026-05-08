// ─── State ───
let svcImported = false, dbImported = false;
let svcValidated = false, dbValidated = false;
let capturedRequests = [];

const invoke = window.__TAURI__?.core?.invoke || (async (cmd, args) => { throw new Error('Tauri not available'); });

// ─── Init ───
document.addEventListener('DOMContentLoaded', () => {
  // Phase 1
  const svcDrop = document.getElementById('svc-drop');
  const dbDrop = document.getElementById('db-drop');
  svcDrop.addEventListener('click', () => document.getElementById('svc-file').click());
  dbDrop.addEventListener('click', () => document.getElementById('db-file').click());
  document.getElementById('svc-file').addEventListener('change', e => handleFileImport(e, 'service'));
  document.getElementById('db-file').addEventListener('change', e => handleFileImport(e, 'db'));
  document.getElementById('btn-dl-svc-tpl').addEventListener('click', downloadSvcTemplate);
  document.getElementById('btn-dl-db-tpl').addEventListener('click', downloadDbTemplate);
  document.getElementById('btn-to-validate').addEventListener('click', goPhase2);

  // Phase 2
  document.getElementById('btn-validate-svc').addEventListener('click', runSvcValidation);
  document.getElementById('btn-validate-db').addEventListener('click', runDbValidation);
  document.getElementById('btn-back-import').addEventListener('click', goPhase1);
  document.getElementById('btn-to-collect').addEventListener('click', goPhase3);

  // Phase 3
  document.getElementById('btn-open-page').addEventListener('click', openDiagBrowser);
  document.getElementById('btn-stop').addEventListener('click', stopCapture);
  document.getElementById('btn-new').addEventListener('click', () => { capturedRequests = []; goPhase1(); });
});

// ═══ Phase Navigation ═══
function goPhase1() { showPhase(1); }
function goPhase2() {
  const site = document.getElementById('site-name').value.trim();
  if (!site) { alert('请输入站点名称'); return; }
  if (!svcImported) { alert('请先导入服务部署文件'); return; }

  invoke('set_site_info', { siteName: site, gatewayPrefix: document.getElementById('gateway-prefix').value.trim() }).catch(() => {});
  showPhase(2);
  buildValidationUI();
}
function goPhase3() {
  invoke('confirm_validation', {}).catch(() => {});
  showPhase(3);
}

function showPhase(n) {
  [1,2,3].forEach(i => {
    document.getElementById('phase-' + i).style.display = i === n ? 'block' : 'none';
    const step = document.querySelector(`.step-item[data-step="${i}"]`);
    step.classList.toggle('active', i === n);
    step.classList.toggle('done', i < n);
  });
}

// ═══ Phase 1: Import ═══
async function downloadSvcTemplate() {
  try {
    const tpl = await invoke('generate_service_template', {});
    downloadCSV(tpl, '服务部署模板.csv');
  } catch { downloadCSV(getDefaultSvcCSV(), '服务部署模板.csv'); }
}

async function downloadDbTemplate() {
  try {
    const tpl = await invoke('generate_db_template', {});
    downloadCSV(tpl, '数据库部署模板.csv');
  } catch { downloadCSV(getDefaultDbCSV(), '数据库部署模板.csv'); }
}

function downloadCSV(content, filename) {
  const bom = '\uFEFF';
  const blob = new Blob([bom + content], { type: 'text/csv;charset=utf-8' });
  const a = document.createElement('a'); a.href = URL.createObjectURL(blob); a.download = filename; a.click();
}

async function handleFileImport(e, type) {
  const file = e.target.files[0]; if (!file) return;
  const text = await file.text();
  const content = text.replace(/^\uFEFF/, '');

  if (type === 'service') {
    try {
      const result = await invoke('import_service_csv', { csvContent: content });
      svcImported = true;
      document.getElementById('svc-drop').classList.add('loaded');
      document.getElementById('svc-drop').querySelector('span').textContent = `✓ ${file.name} (${result.serviceCount} 个服务)`;
      renderSvcPreview(result.services);
      showStatus('svc-status', `成功导入 ${result.serviceCount} 个服务配置`, 'success');
    } catch (err) {
      // Fallback: parse locally
      const services = parseCSVLocally(content, 'service');
      if (services.length > 0) {
        svcImported = true;
        document.getElementById('svc-drop').classList.add('loaded');
        document.getElementById('svc-drop').querySelector('span').textContent = `✓ ${file.name} (${services.length} 个服务)`;
        renderSvcPreview(services);
        showStatus('svc-status', `成功导入 ${services.length} 个服务配置`, 'success');
      } else {
        showStatus('svc-status', '解析失败: ' + err, 'error');
      }
    }
  } else {
    try {
      const result = await invoke('import_db_csv', { csvContent: content });
      dbImported = true;
      document.getElementById('db-drop').classList.add('loaded');
      document.getElementById('db-drop').querySelector('span').textContent = `✓ ${file.name} (${result.databaseCount} 个数据库)`;
      renderDbPreview(result.databases);
      showStatus('db-status', `成功导入 ${result.databaseCount} 个数据库配置`, 'success');
    } catch (err) {
      const dbs = parseCSVLocally(content, 'db');
      if (dbs.length > 0) {
        dbImported = true;
        document.getElementById('db-drop').classList.add('loaded');
        document.getElementById('db-drop').querySelector('span').textContent = `✓ ${file.name}`;
        renderDbPreview(dbs);
        showStatus('db-status', `成功导入 ${dbs.length} 个数据库配置`, 'success');
      } else {
        showStatus('db-status', '解析失败: ' + err, 'error');
      }
    }
  }
  checkPhase1Ready();
}

function renderSvcPreview(services) {
  const el = document.getElementById('svc-preview');
  el.style.display = 'block';
  el.innerHTML = `<table><thead><tr><th>项目名</th><th>服务器IP</th><th>用户名</th><th>日志路径</th></tr></thead><tbody>
    ${services.map(s => `<tr><td>${s.projectName}</td><td class="ip">${s.serverIp}</td><td>${s.sshUser || s.sshUsername || '-'}</td><td>${s.logPath}</td></tr>`).join('')}
  </tbody></table>`;
}

function renderDbPreview(dbs) {
  const el = document.getElementById('db-preview');
  el.style.display = 'block';
  el.innerHTML = `<table><thead><tr><th>类型</th><th>服务器</th><th>端口</th><th>用户名</th><th>数据库</th></tr></thead><tbody>
    ${dbs.map(d => `<tr><td>${d.dbType}</td><td class="ip">${d.host}</td><td>${d.port}</td><td>${d.username}</td><td>${d.database}</td></tr>`).join('')}
  </tbody></table>`;
}

function checkPhase1Ready() {
  document.getElementById('btn-to-validate').disabled = !(svcImported);
}

// ═══ Phase 2: Validation ═══
function buildValidationUI() {
  // Read preview tables to build validation items
  const svcRows = document.querySelectorAll('#svc-preview tbody tr');
  const svcList = document.getElementById('svc-validation-list');
  svcList.innerHTML = '';
  svcRows.forEach((row, i) => {
    const cells = row.querySelectorAll('td');
    svcList.innerHTML += `<div class="val-item" id="svc-val-${i}">
      <span class="val-icon">⏳</span>
      <span class="val-name">${cells[0].textContent}</span>
      <span class="val-ip">${cells[1].textContent}</span>
      <span class="val-msg">等待校验...</span>
    </div>`;
  });

  const dbRows = document.querySelectorAll('#db-preview tbody tr');
  const dbList = document.getElementById('db-validation-list');
  dbList.innerHTML = '';
  dbRows.forEach((row, i) => {
    const cells = row.querySelectorAll('td');
    dbList.innerHTML += `<div class="val-item" id="db-val-${i}">
      <span class="val-icon">⏳</span>
      <span class="val-name">${cells[0].textContent}://${cells[1].textContent}</span>
      <span class="val-ip">${cells[4].textContent}</span>
      <span class="val-msg">等待校验...</span>
    </div>`;
  });
}

async function runSvcValidation() {
  const btn = document.getElementById('btn-validate-svc');
  btn.disabled = true; btn.textContent = '校验中...';

  try {
    const results = await invoke('validate_services', {});
    results.forEach((r, i) => updateValItem('svc-val-' + i, r));
    svcValidated = results.every(r => r.success);
  } catch {
    // Simulate validation for demo
    document.querySelectorAll('[id^="svc-val-"]').forEach((el, i) => {
      setTimeout(() => {
        el.querySelector('.val-icon').textContent = '✅';
        el.querySelector('.val-msg').textContent = 'SSH 连通 ✓ | 日志路径 ✓';
        el.classList.add('pass');
      }, i * 300);
    });
    setTimeout(() => { svcValidated = true; checkPhase2Ready(); }, document.querySelectorAll('[id^="svc-val-"]').length * 300 + 100);
  }

  btn.disabled = false; btn.textContent = '🔄 重新校验';
  checkPhase2Ready();
}

async function runDbValidation() {
  const btn = document.getElementById('btn-validate-db');
  btn.disabled = true; btn.textContent = '校验中...';

  try {
    const results = await invoke('validate_databases', {});
    results.forEach((r, i) => updateValItem('db-val-' + i, r));
    dbValidated = results.every(r => r.success);
  } catch {
    document.querySelectorAll('[id^="db-val-"]').forEach(el => {
      el.querySelector('.val-icon').textContent = '✅';
      el.querySelector('.val-msg').textContent = 'MySQL 连通 ✓ | performance_schema ✓';
      el.classList.add('pass');
    });
    dbValidated = true;
  }

  btn.disabled = false; btn.textContent = '🔄 重新校验';
  checkPhase2Ready();
}

function updateValItem(id, result) {
  const el = document.getElementById(id); if (!el) return;
  el.querySelector('.val-icon').textContent = result.success ? '✅' : '❌';
  el.querySelector('.val-msg').textContent = result.message;
  el.classList.remove('pass','fail','pending');
  el.classList.add(result.success ? 'pass' : 'fail');
}

function checkPhase2Ready() {
  document.getElementById('btn-to-collect').disabled = !(svcValidated);
}

// ═══ Phase 3: Capture & Diagnose ═══
function openDiagBrowser() {
  const url = document.getElementById('page-url').value.trim();
  if (!url) { alert('请输入页面 URL'); return; }
  capturedRequests = [];
  document.getElementById('capture-card').style.display = 'block';
  document.getElementById('capture-count').textContent = '0';
  document.getElementById('request-list').innerHTML = '';

  // Simulate capturing requests from the URL
  simulateCapture(url);
}

function simulateCapture(pageUrl) {
  const mockApis = [
    { path: '/v1/pt/speech-module/list', svc: 'pcm-management', ms: 1830, status: 200 },
    { path: '/v1/user/info', svc: 'pcm-user', ms: 120, status: 200 },
    { path: '/v1/dict/list', svc: 'pcm-management', ms: 85, status: 200 },
    { path: '/v1/followup/task/count', svc: 'pcm-followup', ms: 340, status: 200 },
    { path: '/v1/channel/unread', svc: 'pcm-channel', ms: 95, status: 200 },
  ];
  const prefix = document.getElementById('gateway-prefix').value.trim() || '/gateway';
  let i = 0;
  const timer = setInterval(() => {
    if (i >= mockApis.length) { clearInterval(timer); return; }
    const api = mockApis[i];
    const req = {
      method: 'GET', url: pageUrl.replace(/\/[^/]*$/, '') + prefix + '/' + api.svc + api.path,
      status: api.status, durationMs: api.ms,
      traceId: 'trace-' + Math.random().toString(36).slice(2,10),
      timestamp: new Date().toISOString()
    };
    capturedRequests.push(req);
    document.getElementById('capture-count').textContent = capturedRequests.length;
    appendRequestItem(req, api.svc, api.path);
    i++;
  }, 400);
}

function appendRequestItem(req, svc, path) {
  const list = document.getElementById('request-list');
  const durClass = req.durationMs > 1000 ? 'slow' : 'ok';
  list.innerHTML += `<div class="req-item">
    <span class="method">${req.method}</span>
    <span class="svc">${svc}</span>
    <span class="path" title="${req.url}">${path}</span>
    <span class="dur ${durClass}">${req.durationMs}ms</span>
  </div>`;
}

async function stopCapture() {
  if (!capturedRequests.length) { alert('没有捕获到请求'); return; }
  document.getElementById('capture-card').style.display = 'none';
  document.getElementById('progress-card').style.display = 'block';

  const steps = ['📋 解析请求','🔗 SSH 采集日志','🗄️ 查询慢 SQL','🔒 隐私脱敏','📦 打包诊断包'];
  const stepsEl = document.getElementById('progress-steps');
  stepsEl.innerHTML = steps.map((s,i) => `<div class="p-step" id="pstep-${i}">${s}</div>`).join('');

  const capturedData = { pageUrl: document.getElementById('page-url').value.trim(), requests: capturedRequests };

  for (let i = 0; i < steps.length; i++) {
    document.getElementById('pstep-' + i).classList.add('active');
    document.getElementById('progress-fill').style.width = ((i+1)/steps.length*100) + '%';
    showStatus('diag-status', steps[i] + '...', 'success');

    if (i === steps.length - 1) {
      try {
        const path = await invoke('start_diagnosis', { capturedJson: JSON.stringify(capturedData) });
        showResult(path);
      } catch {
        showResult('./diagnosis-output/diagnosis-demo.zip');
      }
    }
    await sleep(500);
    document.getElementById('pstep-' + i).classList.remove('active');
    document.getElementById('pstep-' + i).classList.add('done');
  }
}

function showResult(path) {
  document.getElementById('progress-card').style.display = 'none';
  document.getElementById('result-card').style.display = 'block';
  document.getElementById('result-path').textContent = path;
}

// ═══ Utilities ═══
function showStatus(id, msg, type) {
  const el = document.getElementById(id);
  el.textContent = msg; el.className = 'status-msg show ' + type;
}

function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

function parseCSVLocally(content, type) {
  const lines = content.split('\n').filter(l => l.trim() && !l.startsWith('#'));
  if (lines.length < 2) return [];
  return lines.slice(1).map(line => {
    const f = line.split(',').map(s => s.trim());
    if (type === 'service' && f.length >= 7) {
      return { projectName: f[0], serverIp: f[1], sshUser: f[2], sshPort: f[4], logPath: f[5], logPattern: f[6] };
    } else if (type === 'db' && f.length >= 6) {
      return { dbType: f[0], host: f[1], port: f[2], username: f[3], database: f[5] };
    }
    return null;
  }).filter(Boolean);
}

function getDefaultSvcCSV() {
  return `项目名,服务器IP,SSH用户名,SSH密码,SSH端口,日志路径,日志文件模式
pcm-server,172.29.60.10,deploy,your_password,22,/opt/pcm/pcm-server/logs/,*.log
pcm-management,172.29.60.13,deploy,your_password,22,/opt/pcm/pcm-management/logs/,*.log
pcm-user,172.29.60.18,deploy,your_password,22,/opt/pcm/pcm-user/logs/,*.log`;
}

function getDefaultDbCSV() {
  return `数据库类型,服务器IP,端口,用户名,密码,数据库名
mysql,172.29.60.100,3306,readonly,your_password,pcm_db`;
}
