use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

static REDIRECT_TARGET: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 诊断窗口捕获的请求数据（旧 custom protocol 路径会写入这里，Windows 外部页主要走标题中转）
#[derive(Default)]
pub struct CapturedDataStore {
    pub data: Mutex<Option<String>>,
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
  var _origTitle = document.title || '';

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

  function _requestKey(req) {
    return [
      req.method || '',
      req.url || '',
      req.status == null ? '' : String(req.status),
      req.timestamp || '',
      req.requestType || ''
    ].join('|');
  }

  function _ensureTopStores() {
    if (!_isTop) return;
    window.__diag_frame_requests = window.__diag_frame_requests || {};
    window.__diag_frame_pages = window.__diag_frame_pages || {};
  }

  function _mergeFramePayload(payload) {
    if (!_isTop || !payload) return;
    _ensureTopStores();
    var frameId = payload.frameId || 'unknown';
    window.__diag_frame_requests[frameId] = Array.isArray(payload.requests) ? payload.requests : [];
    if (payload.pageUrl) {
      window.__diag_frame_pages[frameId] = payload.pageUrl;
    }
  }

  function _allRequests() {
    if (!_isTop) return window.__diag_requests || [];
    _ensureTopStores();
    _mergeFramePayload({
      frameId: _frameId,
      pageUrl: window.__diag_page_url || location.href,
      requests: window.__diag_requests || []
    });

    var seen = {};
    var all = [];
    Object.keys(window.__diag_frame_requests).forEach(function(frameId) {
      (window.__diag_frame_requests[frameId] || []).forEach(function(req) {
        var key = _requestKey(req);
        if (!seen[key]) {
          seen[key] = true;
          all.push(req);
        }
      });
    });
    return all;
  }

  function _setTopCountTitle(count) {
    if (!_isTop) return;
    try {
      var base = (_origTitle || document.title || '').replace(/^\[DIAG:\d+\]\s*/, '');
      document.title = '[DIAG:' + count + '] ' + base;
    } catch(e) {}
  }

  function _publishCaptureState() {
    var payload = {
      type: _frameMessageType,
      frameId: _frameId,
      pageUrl: window.__diag_page_url || location.href,
      requests: window.__diag_requests || []
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

  function _resetLocalCapture() {
    window.__diag_requests = [];
    window.__diag_page_url = location.href;
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
        window.frames[i].postMessage({ type: _resetMessageType }, '*');
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
        window.__resetDiagCapture();
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
            requests: parsed.requests
          });
        } catch(e) {}
        window.__sendDiagData();
      }
    });
  } catch(e) {}

  // ═══ 拦截 fetch ═══
  window.fetch = async function(input, init) {
    const start = performance.now();
    let url = typeof input === 'string' ? input : (input instanceof Request ? input.url : String(input));
    let method = (init && init.method) || (input instanceof Request ? input.method : 'GET');

    try {
      const resp = await _origFetch.call(window, input, init);
      if (_shouldCapture(url)) {
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
          timestamp: new Date().toISOString(),
          requestType: 'fetch',
          responseSize: size
        });
        _notifyCount();
      }
      return resp;
    } catch (err) {
      if (_shouldCapture(url)) {
        const duration = Math.round(performance.now() - start);
        window.__diag_requests.push({
          method: method.toUpperCase(),
          url: url,
          status: 0,
          durationMs: duration,
          traceId: null,
          timestamp: new Date().toISOString(),
          requestType: 'fetch',
          responseSize: null
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
      this.addEventListener('loadend', function() {
        var url = this.__diag.url;
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
          timestamp: new Date().toISOString(),
          requestType: 'xhr',
          responseSize: size
        });
        _notifyCount();
      });
    }
    _origSend.apply(this, arguments);
  };

  // ═══ 获取采集数据 ═══
  window.__getDiagData = function() {
    return JSON.stringify({
      pageUrl: window.__diag_page_url || location.href,
      requests: _isTop ? _allRequests() : (window.__diag_requests || [])
    });
  };

  // ═══ 发送采集数据到 Rust 后端 ═══
  window.__sendDiagData = function() {
    var data = window.__getDiagData();
    try {
      if (_isTop) {
        document.title = '__DIAG_DATA_START__' + data + '__DIAG_DATA_END__';
      } else {
        window.top.postMessage({
          type: _dataMessageType,
          frameId: _frameId,
          data: data
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
  // 外部医院页面不能通过 XHR/fetch 访问 diag:// 自定义协议；WebView2 会按 CORS
  // 拦截。因此 frame 内捕获的数据通过 postMessage 汇总到顶层页，顶层页再把计数
  // 写进标题栏，供 Rust 侧轮询读取。
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

  console.log('[Smart-Diag] API 捕获脚本已注入（XHR + fetch，过滤静态资源，postMessage/title 回传）');
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

    let _window =
        WebviewWindowBuilder::new(app, "diagnostic", diagnostic_window_url(&parsed_url))
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
                tracing::info!("诊断窗口 on_page_load [{:?}] -> {}", payload.event(), loaded);
                let on_target = loaded.host_str().map(|s| s.to_string()) == target_host;
                if on_target {
                    let _ = window.set_title(friendly_title);
                } else {
                    let _ = window
                        .set_title(&format!("诊断浏览器【{}】[加载: {}]", NAV_TAG, loaded));
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
            other => panic!("诊断窗口初始地址不应再停在 tauri.localhost 跳板页: {:?}", other),
        }
    }

    #[test]
    fn diagnostic_script_does_not_send_custom_scheme_requests_from_external_page() {
        assert!(
            !super::DIAGNOSTIC_JS.contains("diag://count"),
            "外部医院页面不能用 XHR/fetch 请求 diag://count，WebView2 会按 CORS 拦截"
        );
        assert!(
            !super::DIAGNOSTIC_JS.contains("diag://collect"),
            "外部医院页面不能用 XHR/fetch 请求 diag://collect，WebView2 会按 CORS 拦截"
        );
        assert!(super::DIAGNOSTIC_JS.contains("postMessage"));
        assert!(super::DIAGNOSTIC_JS.contains("[DIAG:"));
        assert!(super::DIAGNOSTIC_JS.contains("__resetDiagCapture"));
    }
}

/// 触发诊断窗口发送数据（通过 eval 调用 JS；外部页由标题中转回传）
pub fn trigger_data_collection(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("diagnostic")
        .ok_or("诊断窗口未打开，请先打开诊断浏览器")?;

    window
        .eval("window.__sendDiagData()")
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
