use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

static REDIRECT_TARGET: OnceLock<Mutex<Option<String>>> = OnceLock::new();
const MAX_CAPTURE_PAYLOAD_CHARS: usize = 512 * 20_000;

/// 诊断窗口捕获的请求数据。
pub struct CapturedDataStore {
    pub data: Mutex<Option<String>>,
    chunks: Mutex<HashMap<String, CaptureChunks>>,
}

struct CaptureChunks {
    total: usize,
    parts: Vec<Option<String>>,
}

impl Default for CapturedDataStore {
    fn default() -> Self {
        Self {
            data: Mutex::new(None),
            chunks: Mutex::new(HashMap::new()),
        }
    }
}

impl CapturedDataStore {
    pub fn clear(&self) {
        *self.data.lock().unwrap() = None;
        self.chunks.lock().unwrap().clear();
    }

    pub fn store_data(&self, data: String) -> Result<(), String> {
        if data.len() > MAX_CAPTURE_PAYLOAD_CHARS {
            return Err(format!("诊断数据过大: {} chars", data.len()));
        }
        *self.data.lock().unwrap() = Some(data);
        Ok(())
    }

    pub fn store_chunk(
        &self,
        id: String,
        index: usize,
        total: usize,
        data: String,
    ) -> Result<Option<String>, String> {
        const MAX_CHUNKS: usize = 512;
        const MAX_CHUNK_CHARS: usize = 20_000;

        if id.is_empty() {
            return Err("分片 ID 为空".to_string());
        }
        if total == 0 || total > MAX_CHUNKS {
            return Err(format!("分片数量异常: {}", total));
        }
        if index >= total {
            return Err(format!("分片序号越界: {}/{}", index, total));
        }
        if data.len() > MAX_CHUNK_CHARS {
            return Err(format!("单个分片过大: {} chars", data.len()));
        }
        if total.saturating_mul(MAX_CHUNK_CHARS) > MAX_CAPTURE_PAYLOAD_CHARS {
            return Err(format!("分片总量过大: {} chunks", total));
        }

        let mut chunks = self.chunks.lock().unwrap();
        let entry = chunks.entry(id.clone()).or_insert_with(|| CaptureChunks {
            total,
            parts: vec![None; total],
        });
        if entry.total != total {
            chunks.remove(&id);
            return Err("同一分片 ID 的总片数不一致".to_string());
        }

        entry.parts[index] = Some(data);
        if entry.parts.iter().all(|part| part.is_some()) {
            let json = entry
                .parts
                .iter()
                .map(|part| part.as_deref().unwrap_or(""))
                .collect::<String>();
            chunks.remove(&id);
            self.store_data(json.clone())?;
            Ok(Some(json))
        } else {
            Ok(None)
        }
    }
}

/// 诊断 JS 注入脚本 —— 拦截 XHR 和 fetch API 请求（过滤静态资源）
/// 策略：
///   1) XMLHttpRequest.prototype 级别 hook —— 穿透任何 qiankun / micro-app 沙箱
///   2) Object.defineProperty(window, 'fetch') getter —— 阻止沙箱覆盖 fetch
///   3) Performance API 兜底 —— 补全 hook 未能覆盖的请求
const DIAGNOSTIC_JS: &str = r#"
(function() {
  'use strict';

  if (window.__SMART_DIAG_CAPTURE_INSTALLED__) {
    console.log('[Smart-Diag] 已安装，跳过');
    return;
  }
  window.__SMART_DIAG_CAPTURE_INSTALLED__ = true;
  window.__SMART_DIAG_ACTIVE = true;

  window.__diag_requests = window.__diag_requests || [];
  window.__diag_page_url = location.href;

  var _origTitle = document.title || '';
  var _lastReportedCount = -1;

  /* ── 捕获规则 ──────────────────────────────────────────────────────────────
     只记录“带 x-trace（traceId）的请求” —— 业务请求经网关都会带回 X-Trace 响应头，
     而静态资源 / 无关请求没有；用 traceId 是否存在作为唯一过滤条件，既能剔除噪声，
     又保证“网络里有几条带 x-trace 的请求，就采到几条，一条不少”。不做手势窗口/时间窗
     裁剪（否则会漏掉稍晚发出的业务请求）。 */
  var MAX_REQUESTS = 5000;        /* 缓冲区硬上限，纯防御，正常复现远达不到 */

  /* ── traceId 提取 ─────────────────────────────────────────────────────── */
  var _traceNames = [
    'x-trace', 'x-trace-id', 'traceparent', 'trace-id',
    'x-traceid', 'x-b3-traceid', 'x-request-id', 'request-id',
    'x-correlation-id', 'correlation-id'
  ];
  function _findTraceId(headers) {
    if (!headers) return null;
    try {
      if (typeof headers.get === 'function') {
        for (var i = 0; i < _traceNames.length; i++) {
          var v = headers.get(_traceNames[i]);
          if (v) return v;
        }
      } else if (Array.isArray(headers)) {
        for (var i = 0; i < headers.length; i++) {
          if (Array.isArray(headers[i]) && headers[i].length >= 2) {
            var k = headers[i][0], v = headers[i][1];
            if (typeof k === 'string' && _traceNames.indexOf(k.toLowerCase()) !== -1) {
              return v;
            }
          }
        }
      } else if (typeof headers === 'object') {
        var keys = Object.keys(headers);
        for (var j = 0; j < keys.length; j++) {
          if (_traceNames.indexOf(keys[j].toLowerCase()) !== -1 && headers[keys[j]]) {
            return headers[keys[j]];
          }
        }
      }
    } catch(e) {}
    return null;
  }
  function _extractTraceId(url, initH, inputH, respH) {
    var tid = _findTraceId(initH) || _findTraceId(inputH) || _findTraceId(respH);
    if (!tid) {
      try {
        var u = new URL(url, location.href);
        tid = u.searchParams.get('traceId') || u.searchParams.get('trace_id') || u.searchParams.get('x-trace');
      } catch(e) {}
    }
    if (!tid || typeof tid !== 'string') return null;
    var t = tid.trim();
    var parts = t.split('-');
    if (parts.length >= 4 && parts[0] === '00') return parts[1];
    return t;
  }

  /* ── 静态资源过滤 ─────────────────────────────────────────────────────── */
  var _staticExts = /\.(js|css|png|jpg|jpeg|gif|svg|ico|woff|woff2|ttf|eot|map|html|htm)(\?|$)/i;
  function _shouldCapture(url) {
    if (!url || url.indexOf('diag://') === 0) return false;
    try { return !_staticExts.test(new URL(url, location.href).pathname); }
    catch(e) { return false; }
  }
  /* 归一化为绝对 URL —— 拦截到的多为相对地址（/gateway/...），
     下游网关过滤/去重/服务解析都按绝对 URL 处理（url::Url::parse 只认绝对地址），
     不归一化会导致带 traceId 的相对请求被过滤掉，只剩 Performance 兜底的空 trace 条目。 */
  function _absUrl(url) {
    try { return new URL(url, location.href).href; } catch(e) { return url; }
  }

  /* ── Tauri 事件回传 ───────────────────────────────────────────────────── */
  function _emitToRust(name, payload) {
    try {
      var api = window.__TAURI__ && window.__TAURI__.event;
      if (api && typeof api.emit === 'function') {
        api.emit(name, payload).catch(function(){});
        return true;
      }
    } catch(e) {}
    return false;
  }
  function _notifyCount() {
    var count = window.__diag_requests.length;
    if (count === _lastReportedCount) return;
    _lastReportedCount = count;
    _emitToRust('smart-diag-capture-count', {
      value: count, pageUrl: window.__diag_page_url || location.href, t: Date.now()
    });
    try {
      var base = (_origTitle || document.title || '').replace(/^\[DIAG:\d+\]\s*/, '');
      document.title = '[DIAG:' + count + '] ' + base;
    } catch(e) {}
  }
  function _addRequest(req) {
    if (!window.__SMART_DIAG_ACTIVE) return;
    /* 只记录带 x-trace 的请求；无 traceId 的（静态资源、无关请求）直接丢弃。 */
    if (!req || !req.traceId) return;
    if (window.__diag_requests.length >= MAX_REQUESTS) return;
    window.__diag_requests.push(req);
    _notifyCount();
  }

  /* ═══════════════════════════════════════════════════════════════════════
     1. Hook fetch —— Object.defineProperty getter
     qiankun ProxySandbox 的 get trap fallthrough 到 rawWindow。
     defineProperty getter 保证 any 时刻读取 window.fetch 都返回 hook。
     ═══════════════════════════════════════════════════════════════════════ */
  var _realFetch = null;
  try { _realFetch = window.fetch.bind(window); } catch(e) {}

  if (_realFetch) {
    var _hookedFetch = async function diagFetch(input, init) {
      var start = performance.now(), startMs = Date.now();
      var url  = typeof input === 'string' ? input : (input && input.url ? input.url : String(input));
      url = _absUrl(url);
      var meth = ((init && init.method) || (input && input.method) || 'GET').toUpperCase();
      var resp;
      try { resp = await _realFetch(input, init); } catch(err) {
        if (window.__SMART_DIAG_ACTIVE && _shouldCapture(url)) {
          _addRequest({ method: meth, url: url, status: 0,
            durationMs: Math.round(performance.now() - start),
            traceId: _extractTraceId(url, init && init.headers, input && input.headers, null),
            timestamp: new Date(startMs).toISOString(), requestType: 'fetch', responseSize: null });
        }
        throw err;
      }
      if (window.__SMART_DIAG_ACTIVE && _shouldCapture(url)) {
        var tid = _extractTraceId(url, init && init.headers, input && input.headers, resp.headers);
        console.log('[Smart-Diag] fetch:', meth, url, resp.status, 'tid:', tid);
        _addRequest({ method: meth, url: url, status: resp.status,
          durationMs: Math.round(performance.now() - start),
          traceId: tid, timestamp: new Date(startMs).toISOString(),
          requestType: 'fetch', responseSize: null });
      }
      return resp;
    };
    try {
      Object.defineProperty(window, 'fetch', {
        get: function() { return _hookedFetch; },
        set: function(fn) {
          if (fn && fn !== _hookedFetch) {
            _realFetch = typeof fn.bind === 'function' ? fn.bind(window) : fn;
          }
        },
        configurable: true, enumerable: true
      });
      console.log('[Smart-Diag] fetch hook OK (defineProperty)');
    } catch(e) {
      window.fetch = _hookedFetch;
      console.log('[Smart-Diag] fetch hook OK (assign)');
    }
  }

  /* ═══════════════════════════════════════════════════════════════════════
     2. Hook XMLHttpRequest.prototype —— 全局原型，穿透所有沙箱
     ═══════════════════════════════════════════════════════════════════════ */
  (function() {
    var proto = XMLHttpRequest.prototype;
    var _oOpen = proto.open, _oSend = proto.send, _oSetHdr = proto.setRequestHeader;

    proto.open = function(method, url) {
      this.__diagInfo = { method: (method||'GET').toUpperCase(), url: url||'', start:0, startMs:0, reqH:{} };
      return _oOpen.apply(this, arguments);
    };
    proto.setRequestHeader = function(name, value) {
      try { if (this.__diagInfo && name) this.__diagInfo.reqH[name.toLowerCase()] = value; } catch(e){}
      return _oSetHdr.apply(this, arguments);
    };
    proto.send = function() {
      if (this.__diagInfo) {
        this.__diagInfo.start = performance.now();
        this.__diagInfo.startMs = Date.now();
        var self = this;
        this.addEventListener('loadend', function onEnd() {
          var info = self.__diagInfo;
          if (!info || !window.__SMART_DIAG_ACTIVE || !_shouldCapture(info.url)) return;
          var respH = { get: function(n) { try { return self.getResponseHeader(n); } catch(e) { return null; } } };
          var tid = _extractTraceId(info.url, info.reqH, null, respH);
          console.log('[Smart-Diag] XHR:', info.method, info.url, self.status, 'tid:', tid);
          _addRequest({ method: info.method, url: _absUrl(info.url), status: self.status,
            durationMs: Math.round(performance.now() - info.start),
            traceId: tid, timestamp: new Date(info.startMs).toISOString(),
            requestType: 'xhr', responseSize: null });
        }, { once: true });
      }
      return _oSend.apply(this, arguments);
    };
    console.log('[Smart-Diag] XHR prototype hook OK');
  })();

  /* ── 数据导出接口 ─────────────────────────────────────────────────────────
     只导出 hook 捕获到的带 x-trace 的请求；不再用 Performance API 兜底
     （Performance 条目拿不到响应头 → 没有 traceId，只会引入空 trace 噪声）。
     去重按 method+url+traceId，保证同一 traceId 的同一接口只留一条、又不误删不同 trace。 */
  window.__getDiagData = function() {
    var seen = {}, result = [];
    (window.__diag_requests || []).forEach(function(req) {
      var k = [req.method, req.url, req.traceId, req.timestamp].join('|');
      if (!seen[k]) { seen[k] = true; result.push(req); }
    });
    return JSON.stringify({ pageUrl: window.__diag_page_url || location.href, requests: result });
  };

  window.__sendDiagData = function() {
    var data = window.__getDiagData();
    try {
      document.title = '__DIAG_DATA_START__' + data + '__DIAG_DATA_END__';
      var eventApi = window.__TAURI__ && window.__TAURI__.event;
      if (eventApi && typeof eventApi.emit === 'function') {
        eventApi.emit('smart-diag-capture-data', {
          data: data, pageUrl: window.__diag_page_url || location.href, t: Date.now()
        }).catch(function(){});
      }
    } catch(e) {}
    try {
      if (window !== window.top) {
        window.top.postMessage({ type: 'smart-diag-data', data: data }, '*');
      }
    } catch(e) {}
  };

  window.__resetDiagCapture = function() {
    window.__diag_requests = [];
    window.__diag_page_url = location.href;
    _lastReportedCount = -1;
    try { performance.clearResourceTimings(); } catch(e) {}
    _notifyCount();
  };

  /* ── URL 变化追踪（仅更新当前页面 URL，不影响捕获）───────────────────────── */
  (function() {
    var _ps = history.pushState, _rs = history.replaceState;
    history.pushState = function() { var r=_ps.apply(this,arguments); window.__diag_page_url=location.href; return r; };
    history.replaceState = function() { var r=_rs.apply(this,arguments); window.__diag_page_url=location.href; return r; };
    window.addEventListener('popstate', function() { window.__diag_page_url = location.href; });
  })();

  /* ── 周期计数 ─────────────────────────────────────────────────────────── */
  setInterval(function() { try { _notifyCount(); } catch(e){} }, 1000);

  console.log('[Smart-Diag] 注入完成 (XHR prototype + fetch defineProperty)');
})();
"#;




pub(crate) const FORCE_DIAGNOSTIC_COUNT_JS: &str = r#"
(function() {
  try {
    var count = 0;
    if (typeof window.__getDiagData === 'function') {
      try {
        var parsed = JSON.parse(window.__getDiagData());
        count = (parsed && parsed.requests && parsed.requests.length) || 0;
      } catch(e) {}
    } else {
      function isStatic(url) {
        try {
          var u = new URL(url, location.href);
          return /\.(js|css|png|jpg|jpeg|gif|svg|ico|woff|woff2|ttf|eot|map|html|htm)(\?|$)/i.test(u.pathname);
        } catch(e) { return false; }
      }
      function shouldCapture(url) {
        return !!url && url.indexOf('diag://') !== 0 && !isStatic(url);
      }
      function perfCount() {
        var urls = {};
        (window.__diag_requests || []).forEach(function(req) {
          if (req && req.url) urls[req.url] = true;
        });
        try {
          (performance.getEntriesByType('resource') || []).forEach(function(entry) {
            var t = entry.initiatorType || '';
            if ((t === 'fetch' || t === 'xmlhttprequest' || t === 'beacon') && shouldCapture(entry.name)) {
              urls[entry.name] = true;
            }
          });
        } catch(e) {}
        return Object.keys(urls).length;
      }
      count = perfCount();
    }
    
    try {
      var eventApi = window.__TAURI__ && window.__TAURI__.event;
      if (eventApi && typeof eventApi.emit === 'function') {
        eventApi.emit('smart-diag-capture-count', {
          value: count,
          pageUrl: window.__diag_page_url || location.href,
          t: Date.now()
        }).catch(function(e) {
          try { console.warn('[Smart-Diag] 计数事件回传失败:', e); } catch(_) {}
        });
      }
    } catch(e) {}
    try {
      document.title = '[DIAG:' + count + '] ' + String(document.title || '').replace(/^\[DIAG:\d+\]\s*/, '');
    } catch(e) {}
  } catch(e) {}
})();
"#;

pub(crate) const FORCE_DIAGNOSTIC_SNAPSHOT_JS: &str = r#"
(function() {
  try {
    var data = null;
    if (typeof window.__getDiagData === 'function') {
      try {
        data = window.__getDiagData();
      } catch(e) {}
    }
    
    if (!data) {
      function isStatic(url) {
        try {
          var u = new URL(url, location.href);
          return /\.(js|css|png|jpg|jpeg|gif|svg|ico|woff|woff2|ttf|eot|map|html|htm)(\?|$)/i.test(u.pathname);
        } catch(e) { return false; }
      }
      function shouldCapture(url) {
        return !!url && url.indexOf('diag://') !== 0 && !isStatic(url);
      }
      function key(req) {
        return [req.method || '', req.url || '', req.status == null ? '' : String(req.status), req.timestamp || '', req.requestType || ''].join('|');
      }
      function pushUnique(all, seen, req) {
        var k = key(req);
        if (!seen[k]) {
          seen[k] = true;
          all.push(req);
        }
      }
      var all = [];
      var seen = {};
      var skipUrls = {};
      (window.__diag_requests || []).forEach(function(req) {
        if (req && req.url) skipUrls[req.url] = true;
        pushUnique(all, seen, req);
      });
      try {
        (performance.getEntriesByType('resource') || []).forEach(function(entry) {
          var initiator = entry.initiatorType || 'performance';
          if (initiator !== 'fetch' && initiator !== 'xmlhttprequest' && initiator !== 'beacon') return;
          if (!shouldCapture(entry.name) || skipUrls[entry.name]) return;
          var startedAt = Date.now();
          if (performance.timeOrigin && entry.startTime != null) {
            startedAt = Math.round(performance.timeOrigin + entry.startTime);
          }
          pushUnique(all, seen, {
            method: 'GET',
            url: entry.name,
            status: entry.responseStatus || 0,
            durationMs: Math.max(0, Math.round(entry.duration || 0)),
            traceId: null,
            timestamp: new Date(startedAt).toISOString(),
            requestType: initiator,
            responseSize: entry.transferSize || entry.encodedBodySize || entry.decodedBodySize || null
          });
        });
      } catch(e) {}
      data = JSON.stringify({
        pageUrl: window.__diag_page_url || location.href,
        requests: all
      });
    }

    try {
      var eventApi = window.__TAURI__ && window.__TAURI__.event;
      if (eventApi && typeof eventApi.emit === 'function') {
        eventApi.emit('smart-diag-capture-data', {
          data: data,
          pageUrl: window.__diag_page_url || location.href,
          t: Date.now()
        }).catch(function(e) {
          try { console.warn('[Smart-Diag] 快照事件回传失败:', e); } catch(_) {}
        });
        
        var parsed = JSON.parse(data);
        var reqCount = (parsed && parsed.requests && parsed.requests.length) || 0;
        eventApi.emit('smart-diag-capture-count', {
          value: reqCount,
          pageUrl: window.__diag_page_url || location.href,
          t: Date.now()
        }).catch(function(e) {
          try { console.warn('[Smart-Diag] 计数事件回传失败:', e); } catch(_) {}
        });
      }
    } catch(e) {}
    try {
      document.title = '__DIAG_DATA_START__' + data + '__DIAG_DATA_END__';
    } catch(e) {}
  } catch(e) {}
})();
"#;

fn redirect_target_store() -> &'static Mutex<Option<String>> {
    REDIRECT_TARGET.get_or_init(|| Mutex::new(None))
}

pub(crate) fn set_diagnostic_redirect_target(target: String) {
    *redirect_target_store().lock().unwrap() = Some(target);
}

pub(crate) fn diagnostic_redirect_target_json() -> String {
    let target = redirect_target_store()
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    serde_json::json!({ "target": target }).to_string()
}

fn diagnostic_window_url(target: &url::Url) -> WebviewUrl {
    WebviewUrl::External(target.clone())
}

/// 打开诊断浏览器窗口
pub fn open_diagnostic_window(app: &AppHandle, url: &str) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window("diagnostic") {
        let _ = existing.close();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let parsed_url: url::Url = url.parse().map_err(|e| format!("URL 格式无效: {}", e))?;

    const NAV_TAG: &str = "v10";
    let friendly_title =
        "🔍 诊断浏览器【v10】 — 请操作页面复现问题，完成后返回主窗口点击「采集完成」";
    let target_host = parsed_url.host_str().map(|s| s.to_string());
    set_diagnostic_redirect_target(parsed_url.as_str().to_string());

    let _window = WebviewWindowBuilder::new(app, "diagnostic", diagnostic_window_url(&parsed_url))
        .title(friendly_title)
        .inner_size(1280.0, 900.0)
        .center()
        .initialization_script(DIAGNOSTIC_JS)
        .devtools(true)
        .on_navigation(|u| {
            tracing::info!("诊断窗口 on_navigation -> {}", u);
            true
        })
        .on_page_load(move |window, payload| {
            let loaded = payload.url().clone();
            tracing::info!(
                "诊断窗口 on_page_load [{:?}] -> {}",
                payload.event(),
                loaded
            );
            let on_target = loaded.host_str().map(|s| s.to_string()) == target_host;
            if on_target {
                let _ = window.set_title(friendly_title);
            } else {
                let _ = window.set_title(&format!("诊断浏览器【{}】[加载: {}]", NAV_TAG, loaded));
            }
        })
        .build()
        .map_err(|e| format!("创建诊断窗口失败: {}", e))?;

    tracing::info!("诊断浏览器已打开: {}", url);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn redirect_target_json_returns_latest_target_url() {
        super::set_diagnostic_redirect_target(
            "http://172.29.60.151/patient-management/login?next=/gateway/a&name=张三".to_string(),
        );

        let value: serde_json::Value =
            serde_json::from_str(&super::diagnostic_redirect_target_json()).unwrap();
        assert_eq!(
            value["target"].as_str(),
            Some("http://172.29.60.151/patient-management/login?next=/gateway/a&name=张三")
        );
    }

    #[test]
    fn diagnostic_window_uses_target_url_as_initial_url() {
        let target = url::Url::parse("http://172.29.60.151/patient-management").unwrap();

        match super::diagnostic_window_url(&target) {
            tauri::WebviewUrl::External(actual) => assert_eq!(actual, target),
            other => panic!(
                "诊断窗口初始地址不应再停在 tauri.localhost 跳板页: {:?}",
                other
            ),
        }
    }

    #[test]
    fn diagnostic_script_does_not_send_custom_scheme_requests_with_xhr_or_fetch() {
        assert!(
            !super::DIAGNOSTIC_JS.contains("xhr.open('POST', 'diag://"),
            "外部医院页面不能用 XHR 请求 diag://，WebView2 会按 CORS 拦截"
        );
        assert!(
            !super::DIAGNOSTIC_JS.contains("_origFetch.call(window, 'diag://"),
            "外部医院页面不能用 fetch 请求 diag://，WebView2 会按 CORS 拦截"
        );
        assert!(super::DIAGNOSTIC_JS.contains("postMessage"));
        assert!(super::DIAGNOSTIC_JS.contains("[DIAG:"));
        assert!(super::DIAGNOSTIC_JS.contains("__resetDiagCapture"));
    }

    #[test]
    fn diagnostic_scripts_use_tauri_events_instead_of_launching_custom_scheme_urls() {
        for script in [
            super::DIAGNOSTIC_JS,
            super::FORCE_DIAGNOSTIC_COUNT_JS,
            super::FORCE_DIAGNOSTIC_SNAPSHOT_JS,
        ] {
            assert!(
                script.contains("window.__TAURI__") && script.contains("eventApi.emit"),
                "诊断页应通过 Tauri event IPC 回传，避免 Windows WebView2 启动外部协议"
            );
            assert!(
                !script.contains("iframe.src = 'diag://"),
                "Windows WebView2 会拦截 diag:// iframe 导航，导致主窗口计数一直为 0"
            );
            assert!(
                !script.contains("collect-chunk"),
                "IPC 回传不需要 URL 分片，避免再次触发自定义协议导航"
            );
        }
        assert!(super::DIAGNOSTIC_JS.contains("smart-diag-capture-count"));
        assert!(super::DIAGNOSTIC_JS.contains("smart-diag-capture-data"));
    }

    #[test]
    fn diagnostic_script_only_captures_requests_with_trace_id() {
        let js = super::DIAGNOSTIC_JS;
        // 捕获规则：只记录带 x-trace（traceId）的请求 —— _addRequest 必须在无 traceId 时直接返回。
        assert!(
            js.contains("if (!req || !req.traceId) return;"),
            "_addRequest 必须只记录带 traceId 的请求"
        );
        // 不能再用手势窗口裁剪（会漏掉稍晚发出的业务请求，导致“网络里 9 条报告里只有 5 条”）。
        assert!(
            !js.contains("_withinGestureWindow") && !js.contains("__diag_gesture_until"),
            "不应再有手势窗口门控，否则会漏掉带 x-trace 的请求"
        );
        // 不能再用 Performance API 兜底（Performance 条目拿不到响应头 → traceId 恒为 null，只会引入空 trace 噪声）。
        assert!(
            !js.contains("getEntriesByType('resource')"),
            "不应再用 Performance 兜底，否则会引入没有 traceId 的空条目"
        );
    }

    #[test]
    fn diagnostic_capability_allows_remote_event_emit_only() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/diagnostic.json")).unwrap();
        let permissions = capability["permissions"].as_array().unwrap();
        assert!(permissions
            .iter()
            .any(|p| p.as_str() == Some("core:event:allow-emit")));
        assert!(
            !permissions
                .iter()
                .any(|p| p.as_str() == Some("core:event:default")),
            "诊断远程页只需要 emit，不能获得 listen/unlisten 等完整事件权限"
        );
        let urls = capability["remote"]["urls"].as_array().unwrap();
        assert!(urls.iter().any(|u| u.as_str() == Some("http://*:*/*")));
        assert!(urls.iter().any(|u| u.as_str() == Some("https://*:*/*")));
    }

    #[test]
    fn captured_store_reassembles_chunked_payloads() {
        let store = super::CapturedDataStore::default();

        assert_eq!(
            store
                .store_chunk("abc".to_string(), 1, 2, "bar".to_string())
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .store_chunk("abc".to_string(), 0, 2, "foo".to_string())
                .unwrap(),
            Some("foobar".to_string())
        );
        assert_eq!(store.data.lock().unwrap().as_deref(), Some("foobar"));
    }

    #[test]
    fn captured_store_stores_direct_event_payloads() {
        let store = super::CapturedDataStore::default();

        store
            .store_data(r#"{"pageUrl":"http://host","requests":[]}"#.to_string())
            .unwrap();

        assert_eq!(
            store.data.lock().unwrap().as_deref(),
            Some(r#"{"pageUrl":"http://host","requests":[]}"#)
        );
    }
}

/// 触发诊断窗口发送数据（通过 eval 调用 JS；外部页由 Tauri event IPC 回传）
pub fn trigger_data_collection(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("diagnostic")
        .ok_or("诊断窗口未打开，请先打开诊断浏览器")?;

    window
        .eval("try { if (window.__sendDiagData) window.__sendDiagData(); } catch(e) { console.error('[Smart-Diag] send failed:', e); }")
        .map_err(|e| format!("触发数据采集失败: {}", e))?;

    Ok(())
}

/// 关闭诊断窗口
pub fn close_diagnostic_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("diagnostic") {
        let _ = window.close();
        tracing::info!("诊断浏览器已关闭");
    }
}

/// 打开诊断窗口的开发者工具（用于调试空白页等问题）
pub fn open_diagnostic_devtools(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("diagnostic")
        .ok_or("诊断浏览器未打开，请先点击「打开诊断浏览器」")?;
    window.open_devtools();
    Ok(())
}
