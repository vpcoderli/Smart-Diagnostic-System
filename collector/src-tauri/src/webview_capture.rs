use std::sync::Mutex;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

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

  console.log('[Smart-Diag] API 捕获脚本已注入（XHR + fetch，过滤静态资源）');
})();
"#;

/// 打开诊断浏览器窗口
pub fn open_diagnostic_window(app: &AppHandle, url: &str) -> Result<(), String> {
    // 如果已存在诊断窗口，先关闭
    if let Some(existing) = app.get_webview_window("diagnostic") {
        let _ = existing.close();
        // 短暂等待以确保旧窗口资源释放
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let parsed_url: url::Url = url.parse().map_err(|e| format!("URL 格式无效: {}", e))?;

    // 版本标记：用于在标题栏肉眼确认运行的是最新二进制（排除旧安装包干扰）。
    const NAV_TAG: &str = "v5";
    let friendly_title =
        "🔍 诊断浏览器【v5】 — 请操作页面复现问题，完成后返回主窗口点击「采集完成」";
    let target_host = parsed_url.host_str().map(|s| s.to_string());
    // 用 location.replace 触发的「页内导航」JS。注意：实测 Rust 侧 navigate()
    // 对该窗口无效（窗口始终停在 app 自身 tauri.localhost）。location.replace 是
    // 在已加载文档的 JS 上下文里发起的真正页内导航，WebView2 必然执行，是与
    // navigate() 完全不同的通道。{:?} 把 URL 转成合法的 JS 字符串字面量。
    let nav_js = format!("window.location.replace({:?})", parsed_url.as_str());
    // 是否已到达目标页（由 on_page_load 用 payload 的真实地址置位，可靠，
    // 不依赖在 Windows 上不稳的 webview.url()）。
    let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let reached_cb = reached.clone();
    let nav_js_cb = nav_js.clone();
    let _window =
        WebviewWindowBuilder::new(app, "diagnostic", WebviewUrl::External(parsed_url.clone()))
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
                let event = payload.event();
                tracing::info!("诊断窗口 on_page_load [{:?}] -> {}", event, loaded);
                let on_target = loaded.host_str().map(|s| s.to_string()) == target_host;
                if on_target {
                    // 已到目标页：置位、恢复友好标题
                    reached_cb.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = window.set_title(friendly_title);
                } else {
                    // 仍停在 app/空白页：标题暴露真实地址；文档加载完成后在页面
                    // 上下文里用 location.replace 跳到目标（比 navigate() 可靠）。
                    let _ = window
                        .set_title(&format!("诊断浏览器【{}】[加载: {}]", NAV_TAG, loaded));
                    if matches!(event, PageLoadEvent::Finished) {
                        tracing::warn!("诊断窗口停在 {}，eval location.replace 跳转目标", loaded);
                        let _ = window.eval(&nav_js_cb);
                    }
                }
            })
            .build()
            .map_err(|e| format!("创建诊断窗口失败: {}", e))?;

    // 重试式兜底（eval 通道）：
    // 即便 on_page_load 因外部首跳被静默取消而从未对 app 页触发，这个线程也会
    // 周期性地在当前文档里 eval location.replace 把它推向目标。每 1.2s 一次，
    // 一旦 on_page_load 观察到已落在目标 host（reached 置位）即停止，避免对已
    // 到达目标页的窗口反复刷新。总窗口约 12s，覆盖虚拟机 WebView2 冷启动。
    {
        let wnd = _window.clone();
        let nav_js_th = nav_js.clone();
        let reached_th = reached.clone();
        std::thread::spawn(move || {
            for _ in 1..=10u32 {
                std::thread::sleep(std::time::Duration::from_millis(1200));
                if reached_th.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let _ = wnd.eval(&nav_js_th);
            }
        });
    }

    tracing::info!("诊断浏览器已打开: {}", url);
    Ok(())
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
