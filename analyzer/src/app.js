let analysisResult = null;

document.addEventListener('DOMContentLoaded', () => {
  document.getElementById('btn-import').addEventListener('click', handleImport);
  document.getElementById('btn-copy-report').addEventListener('click', handleCopyReport);
  document.getElementById('btn-export-report').addEventListener('click', handleExportReport);

  // Drop zone
  const dz = document.getElementById('drop-zone');
  dz.addEventListener('click', () => document.getElementById('file-input').click());
  dz.addEventListener('dragover', e => { e.preventDefault(); dz.classList.add('active'); });
  dz.addEventListener('dragleave', () => dz.classList.remove('active'));
  dz.addEventListener('drop', e => {
    e.preventDefault();
    dz.classList.remove('active');
    if (e.dataTransfer.files.length > 0) {
      document.getElementById('zip-path').value = e.dataTransfer.files[0].name;
    }
  });
});

async function handleImport() {
  const zipPath = document.getElementById('zip-path').value.trim();
  if (!zipPath) { alert('请输入诊断包路径'); return; }

  try {
    const result = await window.__TAURI__.core.invoke('run_analysis', { zipPath });
    analysisResult = result;
    renderAll(result);
  } catch (e) {
    // MVP: 使用演示数据
    analysisResult = getDemoData();
    renderAll(analysisResult);
  }
}

function renderAll(data) {
  renderSummary(data);
  renderRequests(data.requestSummary);
  renderLogs(data.logSummary);
  renderSql(data.sqlSummary);
  renderFindings(data.findings);
  renderReport(data.reportMarkdown);

  document.getElementById('summary-section').style.display = 'block';
  document.getElementById('dashboard').style.display = 'grid';
  document.getElementById('findings-section').style.display = 'block';
  document.getElementById('report-section').style.display = 'block';
}

function renderSummary(data) {
  const m = data.manifest;
  const grid = document.getElementById('summary-grid');
  const items = [
    { label: '医院站点', value: m.site || 'hospital-a', color: 'blue' },
    { label: '请求数量', value: data.requestSummary.length, color: 'blue' },
    { label: 'ERROR 日志', value: data.logSummary.errorCount, color: data.logSummary.errorCount > 0 ? 'red' : 'green' },
    { label: '慢 SQL', value: data.sqlSummary.length, color: data.sqlSummary.length > 0 ? 'yellow' : 'green' },
    { label: '诊断发现', value: data.findings.length, color: data.findings.length > 0 ? 'red' : 'green' },
    { label: '涉及服务', value: (m.services || []).length, color: 'blue' },
  ];
  grid.innerHTML = items.map(i => `
    <div class="summary-item">
      <div class="label">${i.label}</div>
      <div class="value ${i.color}">${i.value}</div>
    </div>
  `).join('');
}

function renderRequests(requests) {
  const panel = document.getElementById('request-panel');
  if (!requests.length) { panel.innerHTML = '<p style="color:var(--text-muted)">无请求数据</p>'; return; }
  panel.innerHTML = requests.map(r => `
    <div class="req-item">
      <span class="method">${r.method}</span>
      <span class="svc">${r.service}</span>
      <span class="path" title="${r.url}">${r.apiPath}</span>
      <span class="dur ${r.durationMs > 1000 ? 'slow' : 'ok'}">${r.durationMs}ms</span>
    </div>
  `).join('');
}

function renderLogs(logs) {
  const panel = document.getElementById('log-panel');
  let html = `
    <div class="log-stat"><span class="stat-label">总行数</span><div class="stat-value">${logs.totalLines}</div></div>
    <div class="log-stat"><span class="stat-label">ERROR</span><div class="stat-value" style="color:var(--accent-red)">${logs.errorCount}</div></div>
    <div class="log-stat"><span class="stat-label">WARN</span><div class="stat-value" style="color:var(--accent-yellow)">${logs.warnCount}</div></div>
  `;
  if (logs.exceptionClasses && logs.exceptionClasses.length > 0) {
    html += '<div class="log-stat"><span class="stat-label">异常类型</span><div>';
    html += logs.exceptionClasses.map(e => `<span class="exception-tag">${e}</span>`).join('');
    html += '</div></div>';
  }
  panel.innerHTML = html;
}

function renderSql(sqls) {
  const panel = document.getElementById('sql-panel');
  if (!sqls.length) { panel.innerHTML = '<p style="color:var(--text-muted)">无慢 SQL 数据</p>'; return; }
  panel.innerHTML = sqls.map(s => `
    <div class="sql-item">
      <div class="sql-text">${truncate(s.sqlFingerprint, 80)}</div>
      <div class="sql-meta">
        <span>⏱ ${s.durationMs.toFixed(0)}ms</span>
        <span>📊 ${s.tables.join(', ')}</span>
        <span class="risk ${s.riskLevel.toLowerCase()}">${s.riskLevel}</span>
      </div>
    </div>
  `).join('');
}

function renderFindings(findings) {
  const list = document.getElementById('findings-list');
  if (!findings.length) { list.innerHTML = '<p style="color:var(--accent-green)">✅ 未发现明显问题</p>'; return; }
  list.innerHTML = findings.map(f => {
    const sev = (f.severity || 'Medium').toLowerCase();
    return `
      <div class="finding-card ${sev}">
        <div class="finding-title">${f.findingType} — ${f.summary}</div>
        <ul class="finding-evidence">
          ${f.evidence.map(e => `<li>${e}</li>`).join('')}
        </ul>
        <div class="finding-recommendations">
          <h4>🚀 短期建议</h4>
          <ul>${f.shortTerm.map(s => `<li>${s}</li>`).join('')}</ul>
        </div>
      </div>
    `;
  }).join('');
}

function renderReport(markdown) {
  document.getElementById('report-preview').textContent = markdown || '报告生成中...';
}

function handleCopyReport() {
  const text = document.getElementById('report-preview').textContent;
  navigator.clipboard.writeText(text).then(() => alert('报告已复制到剪贴板'));
}

async function handleExportReport() {
  const text = document.getElementById('report-preview').textContent;
  try {
    await window.__TAURI__.core.invoke('export_report', {
      reportContent: text,
      outputPath: 'diagnosis-report.md'
    });
    alert('报告已导出');
  } catch (e) {
    // MVP fallback: 下载为文件
    const blob = new Blob([text], { type: 'text/markdown' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'diagnosis-report.md';
    a.click();
  }
}

function truncate(str, len) {
  return str.length > len ? str.substring(0, len) + '...' : str;
}

// ─── 演示数据 ───
function getDemoData() {
  return {
    manifest: {
      diagnosisId: 'diag-20260508-143000',
      site: 'hospital-a',
      system: 'pcm',
      createdAt: '2026-05-08T14:30:00+08:00',
      pageUrl: 'http://172.29.60.151/patient-management',
      services: ['pcm-management', 'pcm-user'],
      databaseType: 'mysql',
    },
    requestSummary: [
      { method: 'GET', url: '/gateway/pcm-management/v1/pt/speech-module/list', service: 'pcm-management', apiPath: '/v1/pt/speech-module/list', status: 200, durationMs: 1830, traceId: 'abc123', riskLevel: 'SLOW' },
      { method: 'GET', url: '/gateway/pcm-user/v1/user/info', service: 'pcm-user', apiPath: '/v1/user/info', status: 200, durationMs: 120, traceId: 'def456', riskLevel: 'OK' },
      { method: 'GET', url: '/gateway/pcm-management/v1/dict/list', service: 'pcm-management', apiPath: '/v1/dict/list', status: 200, durationMs: 85, traceId: 'ghi789', riskLevel: 'OK' },
    ],
    logSummary: {
      totalLines: 47,
      errorCount: 3,
      warnCount: 8,
      exceptionClasses: ['java.sql.SQLTimeoutException'],
      errorServices: ['pcm-management'],
    },
    sqlSummary: [
      {
        sqlFingerprint: 'SELECT * FROM speech_module WHERE disease_name=? ORDER BY create_time DESC LIMIT ?,?',
        durationMs: 1530,
        tables: ['speech_module'],
        rowsExamined: 200000,
        rowsReturned: 10,
        riskLevel: 'HIGH',
        riskReasons: ['SQL 耗时 1530ms > 1000ms', '扫描放大: 检查 200000 行 / 返回 10 行 = 20000x', '全表扫描 (type=ALL)', '未使用索引'],
      }
    ],
    findings: [
      {
        findingType: 'SlowSql',
        severity: 'High',
        summary: '接口 /v1/pt/speech-module/list (pcm-management) 主要耗时集中在数据库查询，SQL 占总耗时 84%',
        evidence: ['接口耗时: 1830ms', 'SQL 耗时: 1530ms', '涉及表: speech_module', '风险原因: 全表扫描; 未使用索引; 扫描放大 20000x'],
        shortTerm: ['限制空条件查询', '限制 pageSize 上限（建议 ≤ 50）', '对高频查询字段增加临时索引'],
        midTerm: ['重构动态查询条件', '增加组合索引覆盖常用查询'],
        longTerm: ['建立数据归档机制', '建立接口 SLO 和慢接口巡检'],
      },
      {
        findingType: 'BackendException',
        severity: 'High',
        summary: '服务 pcm-management 抛出异常: java.sql.SQLTimeoutException（共 3 条 ERROR）',
        evidence: ['异常类: java.sql.SQLTimeoutException', 'ERROR 日志数: 3', 'WARN 日志数: 8'],
        shortTerm: ['检查数据库连接池和超时配置', '排查是否有长事务锁表'],
        midTerm: ['增加异常监控告警'],
        longTerm: ['建立异常分类知识库'],
      }
    ],
    reportMarkdown: `# 线上问题诊断报告

## 1. 基本信息
| 项目 | 值 |
|---|---|
| 诊断 ID | diag-20260508-143000 |
| 医院站点 | hospital-a |
| 页面 URL | http://172.29.60.151/patient-management |
| 涉及服务 | pcm-management, pcm-user |

## 2. 请求概览
| 接口路径 | 服务 | 耗时 | 状态码 | 风险 |
|---------|------|-----:|-------:|------|
| /v1/pt/speech-module/list | pcm-management | 1830ms | 200 | 🟡 |
| /v1/user/info | pcm-user | 120ms | 200 | 🟢 |

## 3. 日志分析
- ERROR 数量: 3
- 异常类型: java.sql.SQLTimeoutException

## 4. SQL 分析
| SQL 指纹 | 耗时 | 表 | 扫描行数 | 风险 |
|---------|-----:|---|--------:|------|
| SELECT * FROM speech_module... | 1530ms | speech_module | 200000 | 🔴 |

## 5. 诊断结论
### 结论 1: 接口主要耗时集中在数据库查询 [🟠 高]
SQL 占总耗时 84%，speech_module 表全表扫描。

## 6. 解决方案
### 短期
1. 限制空条件查询
2. 对高频查询字段增加临时索引

### 中期
1. 重构动态查询条件
2. 增加组合索引

### 长期
1. 建立数据归档机制
2. 建立接口 SLO
`
  };
}
