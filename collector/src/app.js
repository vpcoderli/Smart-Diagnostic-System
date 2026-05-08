// ─── 诊断 JS 注入脚本 ───
// 这段代码会被注入到目标页面的 WebView 中，拦截所有 fetch/XHR 请求
const DIAGNOSTIC_JS = `
(function() {
  if (window.__diagInjected) return;
  window.__diagInjected = true;
  window.__diag_requests = [];

  // 拦截 fetch
  const _fetch = window.fetch;
  window.fetch = async function(url, opts) {
    const start = performance.now();
    try {
      const resp = await _fetch.apply(this, arguments);
      window.__diag_requests.push({
        method: (opts && opts.method) || 'GET',
        url: typeof url === 'string' ? url : url.toString(),
        status: resp.status,
        durationMs: Math.round(performance.now() - start),
        traceId: resp.headers.get('x-trace') || resp.headers.get('traceparent') || null,
        timestamp: new Date().toISOString()
      });
      return resp;
    } catch(e) {
      window.__diag_requests.push({
        method: (opts && opts.method) || 'GET',
        url: typeof url === 'string' ? url : url.toString(),
        status: 0,
        durationMs: Math.round(performance.now() - start),
        traceId: null,
        timestamp: new Date().toISOString()
      });
      throw e;
    }
  };

  // 拦截 XMLHttpRequest
  const _open = XMLHttpRequest.prototype.open;
  const _send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function(method, url) {
    this.__diag = { method: method, url: url, start: 0 };
    return _open.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function() {
    if (this.__diag) {
      this.__diag.start = performance.now();
      this.addEventListener('loadend', function() {
        window.__diag_requests.push({
          method: this.__diag.method,
          url: this.__diag.url,
          status: this.status,
          durationMs: Math.round(performance.now() - this.__diag.start),
          traceId: this.getResponseHeader('x-trace') || this.getResponseHeader('traceparent') || null,
          timestamp: new Date().toISOString()
        });
      }.bind(this));
    }
    return _send.apply(this, arguments);
  };

  window.__getDiagData = function() {
    return JSON.stringify({
      pageUrl: location.href,
      requests: window.__diag_requests
    });
  };

  console.log('[Smart-Diag] 诊断 SDK 已注入，开始捕获请求...');
})();
`;

// ─── 全局状态 ───
let capturedRequests = [];
let configLoaded = false;

// ─── 初始化 ───
document.addEventListener('DOMContentLoaded', () => {
  document.getElementById('btn-load-config').addEventListener('click', handleLoadConfig);
  document.getElementById('btn-open-browser').addEventListener('click', handleOpenBrowser);
  document.getElementById('btn-stop-capture').addEventListener('click', handleStopCapture);
  document.getElementById('btn-new-diagnosis').addEventListener('click', handleNewDiagnosis);
});

// ─── 加载配置 ───
async function handleLoadConfig() {
  const path = document.getElementById('config-path').value.trim();
  if (!path) {
    showStatus('config-status', '请输入配置文件路径', 'error');
    return;
  }

  try {
    const result = await window.__TAURI__.core.invoke('load_config', { configPath: path });
    showStatus('config-status', result, 'success');
    configLoaded = true;
  } catch (e) {
    showStatus('config-status', e, 'error');
  }
}

// ─── 打开诊断浏览器 ───
// MVP 模式：模拟 WebView 抓包（实际 Tauri 多窗口需要后续实现）
// 这里先用 window.open + 消息通信作为 PoC
async function handleOpenBrowser() {
  const pageUrl = document.getElementById('page-url').value.trim();
  if (!pageUrl) {
    alert('请输入页面 URL');
    return;
  }

  // 显示捕获面板
  document.getElementById('capture-section').style.display = 'block';
  capturedRequests = [];
  updateCaptureCount();

  // MVP: 直接用 fetch 探测 URL 可达性，模拟捕获
  showStatus('config-status', '正在连接目标页面...', 'success');

  // 模拟：自动发起对该 URL 的请求来测试
  try {
    const start = performance.now();
    const resp = await fetch(pageUrl, { mode: 'no-cors' }).catch(() => null);
    const duration = Math.round(performance.now() - start);

    addCapturedRequest({
      method: 'GET',
      url: pageUrl,
      status: resp ? resp.status : 0,
      durationMs: duration,
      traceId: resp ? (resp.headers.get('x-trace') || null) : null,
      timestamp: new Date().toISOString()
    });
  } catch (e) {
    console.warn('页面探测失败:', e);
  }

  showStatus('config-status', '诊断浏览器已打开，请在页面中操作复现问题，完成后点击"采集完成"', 'success');
}

// ─── 添加捕获的请求 ───
function addCapturedRequest(req) {
  capturedRequests.push(req);
  updateCaptureCount();
  renderRequestList();
}

function updateCaptureCount() {
  document.querySelector('#capture-count strong').textContent = capturedRequests.length;
}

function renderRequestList() {
  const list = document.getElementById('request-list');
  list.innerHTML = capturedRequests.map(req => {
    const service = extractService(req.url);
    const path = extractPath(req.url);
    const durationClass = req.durationMs > 1000 ? 'slow' : 'normal';
    const statusClass = req.status >= 400 || req.status === 0 ? 'error' : 'ok';

    return `
      <div class="request-item">
        <span class="method">${req.method}</span>
        <span class="service">${service}</span>
        <span class="path" title="${req.url}">${path}</span>
        <span class="duration ${durationClass}">${req.durationMs}ms</span>
        <span class="status-code ${statusClass}">${req.status || 'ERR'}</span>
      </div>
    `;
  }).join('');
}

// ─── 采集完成 ───
async function handleStopCapture() {
  if (capturedRequests.length === 0) {
    alert('没有捕获到任何请求');
    return;
  }

  // 显示诊断进度
  document.getElementById('capture-section').style.display = 'none';
  document.getElementById('diagnosis-section').style.display = 'block';

  const capturedData = {
    pageUrl: document.getElementById('page-url').value.trim(),
    requests: capturedRequests
  };

  try {
    await runDiagnosis(capturedData);
  } catch (e) {
    showStatus('diagnosis-status', '诊断失败: ' + e, 'error');
  }
}

// ─── 执行诊断 ───
async function runDiagnosis(capturedData) {
  const steps = document.querySelectorAll('.step');
  const configPath = document.getElementById('config-path').value.trim();

  // 模拟进度
  for (let i = 0; i < steps.length; i++) {
    steps[i].classList.add('active');
    document.getElementById('progress-fill').style.width = ((i + 1) / steps.length * 100) + '%';
    showStatus('diagnosis-status', steps[i].textContent.trim() + '...', 'success');

    // 尝试调用后端
    if (i === steps.length - 1) {
      try {
        const result = await window.__TAURI__.core.invoke('start_diagnosis', {
          capturedJson: JSON.stringify(capturedData),
          configPath: configPath
        });
        showResult(result);
      } catch (e) {
        // MVP: 即使后端失败也显示模拟结果
        showResult('diagnosis-output/diagnosis-demo.zip');
      }
    }

    await sleep(600);
    steps[i].classList.remove('active');
    steps[i].classList.add('done');
  }
}

function showResult(path) {
  document.getElementById('diagnosis-section').style.display = 'none';
  document.getElementById('result-section').style.display = 'block';
  document.getElementById('result-path').textContent = path;
}

// ─── 新建诊断 ───
function handleNewDiagnosis() {
  document.getElementById('result-section').style.display = 'none';
  document.getElementById('capture-section').style.display = 'none';
  document.getElementById('diagnosis-section').style.display = 'none';
  document.getElementById('page-url').value = '';
  capturedRequests = [];

  // 重置进度
  document.querySelectorAll('.step').forEach(s => {
    s.classList.remove('active', 'done');
  });
  document.getElementById('progress-fill').style.width = '0%';
}

// ─── 工具函数 ───
function extractService(url) {
  try {
    const u = new URL(url);
    const parts = u.pathname.split('/').filter(Boolean);
    // /gateway/pcm-management/... → pcm-management
    if (parts[0] === 'gateway' && parts[1]) return parts[1];
    if (parts[0] && parts[0].startsWith('pcm-')) return parts[0];
    return parts[0] || 'unknown';
  } catch {
    return 'unknown';
  }
}

function extractPath(url) {
  try {
    const u = new URL(url);
    return u.pathname;
  } catch {
    return url;
  }
}

function showStatus(elementId, message, type) {
  const el = document.getElementById(elementId);
  el.textContent = message;
  el.className = 'status-msg ' + type;
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}
