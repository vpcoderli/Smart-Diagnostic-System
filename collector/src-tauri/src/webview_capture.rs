use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

static REDIRECT_TARGET: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 诊断窗口捕获的请求数据（通过 custom protocol 回传）
#[derive(Default)]
pub struct CapturedDataStore {
    pub data: Mutex<Option<String>>,
}

/// 诊断 JS 注入脚本 —— 拦截 XHR 和 fetch API 请求（过滤静态资源）
const DIAGNOSTIC_JS: &str = r#"
(function() {
  'use strict';

  const _origXHR = XMLHttpRequest;
  const _origFetch = window.fetch;

  window.__diag_requests = [];
  window.__diag_page_url = location.href;

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
      requests: window.__diag_requests || []
    });
  };

  // ═══ 发送采集数据到 Rust 后端 ═══
  window.__sendDiagData = function() {
    var data = window.__getDiagData();
    // 方式1: XHR POST（某些平台可能被阻止）
    try {
      var xhr = new _origXHR();
      xhr.open('POST', 'diag://collect', true);
      xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.send(data);
    } catch(e) {
      console.warn('[Smart-Diag] XHR to diag://collect failed:', e);
    }
    // 方式2: fetch（作为备选）
    try {
      _origFetch.call(window, 'diag://collect', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: data,
        mode: 'no-cors'
      }).catch(function(){});
    } catch(e) {}
  };

  // ═══ 通知计数更新 ═══
  var _origTitle = document.title;
  function _notifyCount() {
    var count = window.__diag_requests.length;
    // 主通道: XHR 到 custom protocol
    try {
      var xhr = new _origXHR();
      xhr.open('POST', 'diag://count', true);
      xhr.setRequestHeader('Content-Type', 'text/plain');
      xhr.send(String(count));
    } catch(e) {}
    // 备用通道: 在标题中嵌入计数，供 Rust 侧直接读取
    try {
      document.title = '[DIAG:' + count + '] ' + (_origTitle || '');
    } catch(e) {}
  }

  // ═══ 页面导航时更新 URL ═══
  var _pushState = history.pushState;
  var _replaceState = history.replaceState;
  history.pushState = function() {
    _pushState.apply(this, arguments);
    window.__diag_page_url = location.href;
  };
  history.replaceState = function() {
    _replaceState.apply(this, arguments);
    window.__diag_page_url = location.href;
  };
  window.addEventListener('popstate', function() {
    window.__diag_page_url = location.href;
  });

  // ═══ 周期性自动回传 ═══
  // 实测 Windows 上 Rust→webview 的 eval 通道可能失效，导致主窗口点「采集完成」
  // 时 Rust 触发的 __sendDiagData() 送不达。这里让页面自身每 2s 主动把已采集数据
  // POST 到 diag://collect，使采集不依赖 Rust eval。仅在有数据时发送，避免用空数据
  // 覆盖已采集结果。
  setInterval(function() {
    try {
      if (window.__diag_requests && window.__diag_requests.length > 0) {
        window.__sendDiagData();
      }
    } catch(e) {}
  }, 2000);

  console.log('[Smart-Diag] API 捕获脚本已注入（XHR + fetch，过滤静态资源，含周期回传）');
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
}

/// 触发诊断窗口发送数据（通过 eval 调用 JS → custom protocol）
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
