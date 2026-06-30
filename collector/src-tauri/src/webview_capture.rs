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
        let incoming_count = captured_request_count(&data);
        let mut stored = self.data.lock().unwrap();
        let stored_count = stored
            .as_deref()
            .and_then(captured_request_count)
            .unwrap_or(0);
        if incoming_count == Some(0) && stored_count > 0 {
            return Ok(());
        }
        *stored = Some(data);
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

fn captured_request_count(data: &str) -> Option<usize> {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()?
        .get("requests")?
        .as_array()
        .map(|requests| requests.len())
}

/// 诊断 JS 注入脚本 —— 拦截 XHR 和 fetch API 请求（过滤静态资源）
const DIAGNOSTIC_JS: &str = r#"
(function() {
  'use strict';

  if (window.__SMART_DIAG_CAPTURE_INSTALLED__) {
    console.log('[Smart-Diag] API 捕获脚本已存在，跳过重复注入');
    return;
  }
  window.__SMART_DIAG_CAPTURE_INSTALLED__ = true;

  const _origXHR = XMLHttpRequest;
  const _origFetch = window.fetch;

  window.__diag_requests = window.__diag_requests || [];
  window.__diag_page_url = location.href;
  var _isTop = false;
  try { _isTop = window === window.top; } catch(e) { _isTop = false; }
  var _frameId = 'frame-' + Date.now() + '-' + Math.random().toString(36).slice(2);
  var _frameMessageType = 'smart-diag-frame-requests';
  var _dataMessageType = 'smart-diag-data-ready';
  var _resetMessageType = 'smart-diag-reset';
  var _countEventName = 'smart-diag-capture-count';
  var _dataEventName = 'smart-diag-capture-data';
  var _origTitle = document.title || '';
  var _lastReportedCount = null;
  var _USER_ACTION_WINDOW_MS = 15000;
  var _CAPTURE_SESSION_TTL_MS = 30 * 60 * 1000;
  var _captureStorageKey = 'smart-diag-capture-session';

  function _newSessionId() {
    return 'session-' + Date.now() + '-' + Math.random().toString(36).slice(2);
  }

  function _readStoredCaptureSession() {
    try {
      var raw = sessionStorage.getItem(_captureStorageKey);
      if (!raw) return null;
      var parsed = JSON.parse(raw);
      if (!parsed || parsed.armed !== true) return null;
      if (!parsed.sessionStartedAt || (Date.now() - parsed.sessionStartedAt) > _CAPTURE_SESSION_TTL_MS) {
        sessionStorage.removeItem(_captureStorageKey);
        return null;
      }
      return parsed;
    } catch(e) {
      return null;
    }
  }

  function _persistCaptureSession() {
    try {
      sessionStorage.setItem(_captureStorageKey, JSON.stringify({
        armed: window.__diag_capture_armed === true,
        sessionId: window.__diag_capture_session_id,
        sessionStartedAt: window.__diag_capture_started_at
      }));
    } catch(e) {}
  }

  var _storedSession = _readStoredCaptureSession();
  window.__diag_capture_session_id = window.__diag_capture_session_id || (_storedSession && _storedSession.sessionId) || _newSessionId();
  window.__diag_capture_started_at = window.__diag_capture_started_at || (_storedSession && _storedSession.sessionStartedAt) || Date.now();
  window.__diag_capture_started_perf = window.__diag_capture_started_perf == null
    ? performance.now()
    : window.__diag_capture_started_perf;
  if (window.__diag_capture_armed !== true) {
    window.__diag_capture_armed = false;
    if (_storedSession && _storedSession.armed === true) {
      window.__diag_capture_armed = !!(_storedSession && _storedSession.armed === true);
    }
  }
  window.__diag_last_user_action_at = window.__diag_last_user_action_at || 0;

  // ═══ 静态资源过滤 ═══
  var _staticExts = /\.(js|css|png|jpg|jpeg|gif|svg|ico|woff|woff2|ttf|eot|map|html|htm)(\?|$)/i;
  function _isStaticResource(url) {
    try {
      var u = new URL(url, location.href);
      return _staticExts.test(u.pathname);
    } catch(e) { return false; }
  }
  function _shouldCapture(url) {
    if (!url || url.indexOf('diag://') === 0) return false;
    if (_isStaticResource(url)) return false;
    return true;
  }

  function _markUserAction() {
    if (!window.__diag_capture_armed) return;
    window.__diag_last_user_action_at = Date.now();
  }

  function _shouldRecordRequest(startedAtMs) {
    if (!window.__diag_capture_armed) return false;
    var actionAt = window.__diag_last_user_action_at || 0;
    if (!actionAt) return false;
    var startedAt = startedAtMs || Date.now();
    if (startedAt < actionAt) return false;
    return (startedAt - actionAt) <= _USER_ACTION_WINDOW_MS;
  }

  function _requestKey(req) {
    return [
      req.method || '',
      req.url || '',
      req.status == null ? '' : String(req.status),
      req.timestamp || '',
      req.requestType || ''
    ].join('|');
  }

  function _belongsToCurrentSession(req) {
    if (!req) return false;
    if (req.__diag_session_id && req.__diag_session_id !== window.__diag_capture_session_id) return false;
    if (req.capturedAtMs && req.capturedAtMs < window.__diag_capture_started_at) return false;
    return true;
  }

  function _emitToRust(eventName, payload) {
    try {
      var eventApi = window.__TAURI__ && window.__TAURI__.event;
      if (eventApi && typeof eventApi.emit === 'function') {
        eventApi.emit(eventName, payload).catch(function(e) {
          try { console.warn('[Smart-Diag] 事件回传失败:', e); } catch(_) {}
        });
        return true;
      }
    } catch(e) {}
    return false;
  }

  function _sendCountToRust(count) {
    if (_lastReportedCount === count) return;
    _lastReportedCount = count;
    _emitToRust(_countEventName, {
      value: count,
      pageUrl: window.__diag_page_url || location.href,
      t: Date.now()
    });
  }

  function _sendDataToRust(data) {
    _emitToRust(_dataEventName, {
      data: data,
      pageUrl: window.__diag_page_url || location.href,
      t: Date.now()
    });
  }

  function _performanceRequests(skipUrls) {
    var requests = [];
    if (!window.__diag_capture_armed) return requests;
    try {
      var entries = performance.getEntriesByType('resource') || [];
      entries.forEach(function(entry) {
        var initiator = entry.initiatorType || 'performance';
        if (initiator !== 'fetch' && initiator !== 'xmlhttprequest' && initiator !== 'beacon') return;
        if (entry.startTime < window.__diag_capture_started_perf) return;
        if (!_shouldCapture(entry.name)) return;
        if (skipUrls && skipUrls[entry.name]) return;
        var startedAt = Date.now();
        if (performance.timeOrigin && entry.startTime != null) {
          startedAt = Math.round(performance.timeOrigin + entry.startTime);
        }
        if (startedAt < window.__diag_capture_started_at) return;
        if (!_shouldRecordRequest(startedAt)) return;
        requests.push({
          method: 'GET',
          url: entry.name,
          status: entry.responseStatus || 0,
          durationMs: Math.max(0, Math.round(entry.duration || 0)),
          traceId: null,
          timestamp: new Date(startedAt).toISOString(),
          requestType: initiator,
          responseSize: entry.transferSize || entry.encodedBodySize || entry.decodedBodySize || null,
          capturedAtMs: startedAt,
          __diag_session_id: window.__diag_capture_session_id
        });
      });
    } catch(e) {}
    return requests;
  }

  function _localRequestsWithPerformance() {
    if (!window.__diag_capture_armed) return [];
    var seen = {};
    var skipUrls = {};
    var all = [];
    (window.__diag_requests || []).forEach(function(req) {
      if (!_belongsToCurrentSession(req)) return;
      var key = _requestKey(req);
      if (req && req.url) skipUrls[req.url] = true;
      if (!seen[key]) {
        seen[key] = true;
        all.push(req);
      }
    });
    _performanceRequests(skipUrls).forEach(function(req) {
      var key = _requestKey(req);
      if (!seen[key]) {
        seen[key] = true;
        all.push(req);
      }
    });
    return all;
  }

  function _ensureTopStores() {
    if (!_isTop) return;
    window.__diag_frame_requests = window.__diag_frame_requests || {};
    window.__diag_frame_pages = window.__diag_frame_pages || {};
  }

  function _mergeFramePayload(payload) {
    if (!_isTop || !payload) return;
    if (!window.__diag_capture_armed) return;
    if (payload.sessionStartedAt && payload.sessionStartedAt < window.__diag_capture_started_at) return;
    if (
      payload.sessionId &&
      payload.sessionId !== window.__diag_capture_session_id &&
      (!payload.sessionStartedAt || payload.sessionStartedAt < window.__diag_capture_started_at)
    ) return;
    _ensureTopStores();
    var frameId = payload.frameId || 'unknown';
    window.__diag_frame_requests[frameId] = Array.isArray(payload.requests) ? payload.requests : [];
    if (payload.pageUrl) {
      window.__diag_frame_pages[frameId] = payload.pageUrl;
    }
  }

  function _allRequests() {
    if (!window.__diag_capture_armed) return [];
    if (!_isTop) return _localRequestsWithPerformance();
    _ensureTopStores();
    _mergeFramePayload({
      frameId: _frameId,
      pageUrl: window.__diag_page_url || location.href,
      requests: _localRequestsWithPerformance()
    });

    var seen = {};
    var skipUrls = {};
    var all = [];
    Object.keys(window.__diag_frame_requests).forEach(function(frameId) {
      (window.__diag_frame_requests[frameId] || []).forEach(function(req) {
        var key = _requestKey(req);
        if (req && req.url) skipUrls[req.url] = true;
        if (!seen[key]) {
          seen[key] = true;
          all.push(req);
        }
      });
    });
    _performanceRequests(skipUrls).forEach(function(req) {
      var key = _requestKey(req);
      if (!seen[key]) {
        seen[key] = true;
        all.push(req);
      }
    });
    return all;
  }

  function _setTopCountTitle(count) {
    if (!_isTop) return;
    try {
      var base = (_origTitle || document.title || '').replace(/^\[DIAG:\d+\]\s*/, '');
      document.title = '[DIAG:' + count + '] ' + base;
      _sendCountToRust(count);
    } catch(e) {}
  }

  function _publishCaptureState() {
    if (!window.__diag_capture_armed) {
      if (_isTop) _setTopCountTitle(0);
      return;
    }
    var payload = {
      type: _frameMessageType,
      frameId: _frameId,
      pageUrl: window.__diag_page_url || location.href,
      requests: (window.__diag_requests || []).filter(_belongsToCurrentSession),
      sessionId: window.__diag_capture_session_id,
      sessionStartedAt: window.__diag_capture_started_at
    };

    if (_isTop) {
      _mergeFramePayload(payload);
      _setTopCountTitle(_allRequests().length);
      return;
    }

    try {
      window.top.postMessage(payload, '*');
    } catch(e) {}
  }

  function _resetLocalCapture(session) {
    session = session || {};
    window.__diag_capture_session_id = session.sessionId || _newSessionId();
    window.__diag_capture_started_at = session.sessionStartedAt || Date.now();
    window.__diag_capture_started_perf = performance.now();
    window.__diag_capture_armed = true;
    window.__diag_last_user_action_at = 0;
    _persistCaptureSession();
    _lastReportedCount = null;
    window.__diag_requests = [];
    window.__diag_page_url = location.href;
    try { performance.clearResourceTimings(); } catch(e) {}
    if (_isTop) {
      window.__diag_frame_requests = {};
      window.__diag_frame_pages = {};
      _setTopCountTitle(0);
    }
  }

  function _broadcastReset() {
    if (!_isTop) return;
    for (var i = 0; i < window.frames.length; i++) {
      try {
        window.frames[i].postMessage({
          type: _resetMessageType,
          sessionId: window.__diag_capture_session_id,
          sessionStartedAt: window.__diag_capture_started_at
        }, '*');
      } catch(e) {}
    }
  }

  window.__resetDiagCapture = function() {
    _resetLocalCapture();
    if (_isTop) {
      _broadcastReset();
    } else {
      _publishCaptureState();
    }
  };

  try {
    window.addEventListener('message', function(event) {
      var data = event && event.data;
      if (!data || typeof data !== 'object') return;
      if (data.type === _resetMessageType) {
        _resetLocalCapture(data);
        return;
      }
      if (!_isTop) return;
      if (data.type === _frameMessageType) {
        _mergeFramePayload(data);
        _setTopCountTitle(_allRequests().length);
      } else if (data.type === _dataMessageType && data.data) {
        try {
          var parsed = JSON.parse(data.data);
          _mergeFramePayload({
            frameId: data.frameId,
            pageUrl: parsed.pageUrl,
            requests: parsed.requests,
            sessionId: data.sessionId,
            sessionStartedAt: data.sessionStartedAt
          });
        } catch(e) {}
        window.__sendDiagData();
      }
    });
  } catch(e) {}

  try {
    ['pointerdown', 'click', 'dblclick', 'keydown', 'change', 'submit'].forEach(function(name) {
      window.addEventListener(name, _markUserAction, true);
    });
  } catch(e) {}

  // ═══ 拦截 fetch ═══
  window.fetch = async function(input, init) {
    const start = performance.now();
    const requestStartedAtMs = Date.now();
    var shouldRecordRequest = _shouldRecordRequest(requestStartedAtMs);
    let url = typeof input === 'string' ? input : (input instanceof Request ? input.url : String(input));
    let method = (init && init.method) || (input instanceof Request ? input.method : 'GET');

    try {
      const resp = await _origFetch.call(window, input, init);
      if (shouldRecordRequest && _shouldCapture(url)) {
        const duration = Math.round(performance.now() - start);
        let size = null;
        const cl = resp.headers.get('content-length');
        if (cl) size = parseInt(cl, 10);

        window.__diag_requests.push({
          method: method.toUpperCase(),
          url: url,
          status: resp.status,
          durationMs: duration,
          traceId: resp.headers.get('x-trace') || resp.headers.get('x-trace-id') || resp.headers.get('traceparent') || null,
          timestamp: new Date(requestStartedAtMs).toISOString(),
          requestType: 'fetch',
          responseSize: size,
          capturedAtMs: requestStartedAtMs,
          __diag_session_id: window.__diag_capture_session_id
        });
        _notifyCount();
      }
      return resp;
    } catch (err) {
      if (shouldRecordRequest && _shouldCapture(url)) {
        const duration = Math.round(performance.now() - start);
        window.__diag_requests.push({
          method: method.toUpperCase(),
          url: url,
          status: 0,
          durationMs: duration,
          traceId: null,
          timestamp: new Date(requestStartedAtMs).toISOString(),
          requestType: 'fetch',
          responseSize: null,
          capturedAtMs: requestStartedAtMs,
          __diag_session_id: window.__diag_capture_session_id
        });
        _notifyCount();
      }
      throw err;
    }
  };

  // ═══ 拦截 XMLHttpRequest ═══
  var _origOpen = _origXHR.prototype.open;
  var _origSend = _origXHR.prototype.send;

  XMLHttpRequest.prototype.open = function(method, url) {
    this.__diag = { method: method, url: url, start: 0 };
    _origOpen.apply(this, arguments);
  };

  XMLHttpRequest.prototype.send = function() {
    if (this.__diag) {
      this.__diag.start = performance.now();
      this.__diag.requestStartedAtMs = Date.now();
      this.__diag.shouldRecordRequest = _shouldRecordRequest(this.__diag.requestStartedAtMs);
      this.addEventListener('loadend', function() {
        var url = this.__diag.url;
        if (!this.__diag.shouldRecordRequest) return;
        if (!_shouldCapture(url)) return;

        var duration = Math.round(performance.now() - this.__diag.start);
        var traceId = null;
        var size = null;
        try {
          traceId = this.getResponseHeader('x-trace') || this.getResponseHeader('x-trace-id') || this.getResponseHeader('traceparent');
          var cl = this.getResponseHeader('content-length');
          if (cl) size = parseInt(cl, 10);
        } catch(e) {}

        if (!size && this.responseText) {
          size = this.responseText.length;
        }

        window.__diag_requests.push({
          method: this.__diag.method.toUpperCase(),
          url: url,
          status: this.status,
          durationMs: duration,
          traceId: traceId,
          timestamp: new Date(this.__diag.requestStartedAtMs).toISOString(),
          requestType: 'xhr',
          responseSize: size,
          capturedAtMs: this.__diag.requestStartedAtMs,
          __diag_session_id: window.__diag_capture_session_id
        });
        _notifyCount();
      });
    }
    _origSend.apply(this, arguments);
  };

  // ═══ 获取采集数据 ═══
  window.__getDiagData = function() {
    if (!window.__diag_capture_armed) {
      return JSON.stringify({
        pageUrl: window.__diag_page_url || location.href,
        requests: []
      });
    }
    return JSON.stringify({
      pageUrl: window.__diag_page_url || location.href,
      requests: _isTop ? _allRequests() : _localRequestsWithPerformance()
    });
  };

  window.__getDiagCount = function() {
    if (!window.__diag_capture_armed) return 0;
    return (_isTop ? _allRequests() : _localRequestsWithPerformance()).length;
  };

  // ═══ 发送采集数据到 Rust 后端 ═══
  window.__sendDiagData = function() {
    var data = window.__getDiagData();
    try {
      if (_isTop) {
        document.title = '__DIAG_DATA_START__' + data + '__DIAG_DATA_END__';
        _sendDataToRust(data);
      } else {
        window.top.postMessage({
          type: _dataMessageType,
          frameId: _frameId,
          data: data,
          sessionId: window.__diag_capture_session_id,
          sessionStartedAt: window.__diag_capture_started_at
        }, '*');
      }
    } catch(e) {}
  };

  // ═══ 通知计数更新 ═══
  function _notifyCount() {
    _publishCaptureState();
  }

  // ═══ 页面导航时更新 URL ═══
  var _pushState = history.pushState;
  var _replaceState = history.replaceState;
  history.pushState = function() {
    var ret = _pushState.apply(this, arguments);
    window.__diag_page_url = location.href;
    _publishCaptureState();
    return ret;
  };
  history.replaceState = function() {
    var ret = _replaceState.apply(this, arguments);
    window.__diag_page_url = location.href;
    _publishCaptureState();
    return ret;
  };
  window.addEventListener('popstate', function() {
    window.__diag_page_url = location.href;
    _publishCaptureState();
  });

  // ═══ 周期性刷新顶层计数 ═══
  // frame 内捕获的数据通过 postMessage 汇总到顶层页，顶层页再通过 Tauri event IPC
  // 回传给 Rust。标题栏只保留给人工观察，不作为 Windows 的可靠数据通道。
  setInterval(function() {
    try {
      if (_isTop) {
        var count = _allRequests().length;
        if (count > 0) _setTopCountTitle(count);
      } else if (window.__diag_requests && window.__diag_requests.length > 0) {
        _publishCaptureState();
      }
    } catch(e) {}
  }, 1000);

  console.log('[Smart-Diag] API 捕获脚本已注入（XHR + fetch，过滤静态资源，Tauri event 回传）');
})();
"#;

pub(crate) const FORCE_DIAGNOSTIC_COUNT_JS: &str = r#"
(function() {
  try {
    function sendCount(count) {
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
    }
    sendCount(window.__getDiagCount ? window.__getDiagCount() : ((window.__diag_requests || []).length));
  } catch(e) {}
})();
"#;

pub(crate) const FORCE_DIAGNOSTIC_SNAPSHOT_JS: &str = r#"
(function() {
  try {
    var data = window.__getDiagData
      ? window.__getDiagData()
      : JSON.stringify({
          pageUrl: window.__diag_page_url || location.href,
          requests: window.__diag_requests || []
        });
    var parsed = { requests: [] };
    try { parsed = JSON.parse(data); } catch(e) {}
    var count = Array.isArray(parsed.requests) ? parsed.requests.length : 0;
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
        assert!(super::DIAGNOSTIC_JS.contains("performance.getEntriesByType('resource')"));
        assert!(super::DIAGNOSTIC_JS.contains("initiatorType"));
    }

    #[test]
    fn diagnostic_scripts_scope_counts_and_snapshots_to_active_capture_session() {
        assert!(super::DIAGNOSTIC_JS.contains("__diag_capture_session_id"));
        assert!(super::DIAGNOSTIC_JS.contains("__diag_capture_started_at"));
        assert!(super::DIAGNOSTIC_JS.contains("__diag_capture_started_perf"));
        assert!(super::DIAGNOSTIC_JS.contains("req.__diag_session_id"));
        assert!(super::DIAGNOSTIC_JS.contains("payload.sessionId"));
        assert!(super::DIAGNOSTIC_JS.contains("payload.sessionStartedAt"));
        assert!(
            super::DIAGNOSTIC_JS.contains("entry.startTime < window.__diag_capture_started_perf")
        );
        assert!(super::DIAGNOSTIC_JS.contains("window.__getDiagCount"));
        assert!(
            super::FORCE_DIAGNOSTIC_COUNT_JS.contains("window.__getDiagCount"),
            "轮询计数应读取注入脚本的会话计数，不能重新扫描整个 performance timeline"
        );
        assert!(
            super::FORCE_DIAGNOSTIC_SNAPSHOT_JS.contains("window.__getDiagData"),
            "强制快照应复用注入脚本聚合后的 frame 数据，不能用顶层空快照覆盖完整数据"
        );
    }

    #[test]
    fn diagnostic_script_starts_disarmed_until_user_resets_capture() {
        assert!(super::DIAGNOSTIC_JS.contains("window.__diag_capture_armed = false"));
        assert!(super::DIAGNOSTIC_JS.contains("window.__diag_capture_armed = true"));
        assert!(
            super::DIAGNOSTIC_JS.contains("if (!window.__diag_capture_armed) return 0"),
            "诊断浏览器刚打开、登录前不能累计系统初始化或轮询请求"
        );
        assert!(
            super::DIAGNOSTIC_JS.contains("if (!window.__diag_capture_armed) return []"),
            "未点击重置采集前，强制快照也必须返回空请求"
        );
    }

    #[test]
    fn diagnostic_script_records_only_requests_triggered_by_user_actions() {
        assert!(super::DIAGNOSTIC_JS.contains("_USER_ACTION_WINDOW_MS"));
        assert!(super::DIAGNOSTIC_JS.contains("__diag_last_user_action_at"));
        assert!(super::DIAGNOSTIC_JS.contains("function _markUserAction()"));
        assert!(
            super::DIAGNOSTIC_JS.contains("window.addEventListener(name, _markUserAction, true)")
        );
        assert!(super::DIAGNOSTIC_JS.contains("function _shouldRecordRequest(startedAtMs)"));
        assert!(
            super::DIAGNOSTIC_JS
                .contains("var shouldRecordRequest = _shouldRecordRequest(requestStartedAtMs)"),
            "fetch 必须按请求开始时间判断是否由用户操作触发"
        );
        assert!(
            super::DIAGNOSTIC_JS.contains("this.__diag.shouldRecordRequest"),
            "XHR 必须按 send 时刻判断是否由用户操作触发"
        );
        assert!(
            super::DIAGNOSTIC_JS.contains("_shouldRecordRequest(startedAt)"),
            "performance fallback 不能把后台轮询重新捞回快照"
        );
    }

    #[test]
    fn diagnostic_script_keeps_reset_capture_armed_across_navigation() {
        assert!(super::DIAGNOSTIC_JS.contains("_captureStorageKey"));
        assert!(super::DIAGNOSTIC_JS.contains("function _readStoredCaptureSession()"));
        assert!(
            super::DIAGNOSTIC_JS.contains("var _storedSession = _readStoredCaptureSession()")
        );
        assert!(
            super::DIAGNOSTIC_JS
                .contains("window.__diag_capture_armed = !!(_storedSession")
        );
        assert!(super::DIAGNOSTIC_JS.contains("sessionStorage.setItem(_captureStorageKey"));
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

    #[test]
    fn captured_store_keeps_non_empty_payload_when_late_empty_snapshot_arrives() {
        let store = super::CapturedDataStore::default();

        store
            .store_data(
                r#"{"pageUrl":"http://host","requests":[{"method":"GET","url":"http://host/gateway/pcm/a","status":200,"durationMs":10,"timestamp":"2026-06-30T00:00:00Z"}]}"#
                    .to_string(),
            )
            .unwrap();
        store
            .store_data(r#"{"pageUrl":"http://host","requests":[]}"#.to_string())
            .unwrap();

        let stored = store.data.lock().unwrap().clone().unwrap();
        let value: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(value["requests"].as_array().unwrap().len(), 1);
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
