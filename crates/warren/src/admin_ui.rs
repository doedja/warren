//! Static HTML for the hub admin dashboard, served at `/` (behind Basic auth).

pub const DASHBOARD: &str = r###"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>warren admin</title>
<style>
  :root {
    --bg:#0a0b0d; --panel:#131519; --panel2:#0d0f12; --line:#23262e;
    --fg:#e7eaf0; --mut:#828b99; --faint:#565d69;
    --acc:#5fb0ff; --ok:#4ec77a; --warn:#e0a23a; --bad:#e8584c;
    --radius:9px; --radius-sm:6px;
  }
  * { box-sizing:border-box; }
  html { -webkit-text-size-adjust:100%; }
  body {
    margin:0; background:var(--bg); color:var(--fg);
    font:13px/1.5 ui-monospace,"SF Mono",SFMono-Regular,Menlo,Consolas,monospace;
    background-image:radial-gradient(1100px 420px at 78% -8%, rgba(95,176,255,.07), transparent 60%);
    background-attachment:fixed;
  }
  a { color:var(--acc); }
  /* Header */
  header {
    position:sticky; top:0; z-index:5;
    padding:13px 22px; border-bottom:1px solid var(--line);
    display:flex; gap:12px; align-items:center; flex-wrap:wrap;
    background:rgba(10,11,13,.82); backdrop-filter:blur(8px);
  }
  .brand { display:flex; align-items:center; gap:9px; margin-right:4px; }
  .mark { width:18px; height:18px; border-radius:5px; background:linear-gradient(135deg,var(--acc),#2c6fc4); box-shadow:0 0 0 1px rgba(95,176,255,.25), 0 4px 14px -4px rgba(95,176,255,.5); position:relative; }
  .mark::after { content:""; position:absolute; inset:5px 5px auto 5px; height:2px; border-radius:2px; background:rgba(8,12,20,.65); box-shadow:0 4px 0 rgba(8,12,20,.65); }
  h1 { font-size:15px; margin:0; letter-spacing:.02em; font-weight:600; }
  h1 b { color:var(--acc); font-weight:600; }
  .ver { color:var(--faint); font-size:11px; align-self:center; }
  h2 { font-size:12px; color:var(--fg); margin:0 0 3px; letter-spacing:.04em; font-weight:600; display:flex; align-items:center; gap:8px; }
  h2::before { content:""; width:3px; height:13px; border-radius:2px; background:var(--acc); opacity:.8; }
  .desc { color:var(--mut); font-size:12px; margin:0 0 12px; line-height:1.5; max-width:74ch; }
  /* Click-to-expand help (a native title= tooltip never showed for the user). */
  .hint { font-size:12px; margin:-4px 0 10px; }
  .hint summary { color:var(--acc); cursor:pointer; list-style:none; display:inline-flex; align-items:center; gap:6px; width:fit-content; }
  .hint summary::-webkit-details-marker { display:none; }
  .hint summary::before { content:"\25B8"; font-size:10px; }
  .hint[open] summary::before { content:"\25BE"; }
  .hint .hintbody { color:var(--mut); margin-top:6px; line-height:1.5; max-width:74ch; }
  .foot { color:var(--faint); font-size:11px; text-align:center; padding:6px 20px 30px; }
  .foot code { color:var(--mut); }
  .log { background:var(--panel2); border:1px solid var(--line); border-radius:var(--radius-sm); padding:10px 12px; max-height:300px; overflow:auto; margin:0; font-size:11.5px; line-height:1.5; white-space:pre-wrap; word-break:break-word; color:var(--mut); }
  .log .lw { color:var(--warn); }
  .log .le { color:var(--bad); }
  .logf.on { color:var(--fg); border-color:var(--acc); }
  .spacer { flex:1; }
  /* Status dots + pills */
  .dot { display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px; vertical-align:middle; }
  .dot.ok { background:var(--ok); box-shadow:0 0 7px -1px var(--ok); }
  .dot.warn { background:var(--warn); box-shadow:0 0 7px -1px var(--warn); }
  .dot.bad { background:var(--bad); box-shadow:0 0 7px -1px var(--bad); }
  .live { color:var(--ok); font-size:11px; border:1px solid var(--line); border-radius:999px; padding:2px 10px; display:inline-flex; align-items:center; gap:6px; }
  .live::before { content:""; width:6px; height:6px; border-radius:50%; background:currentColor; box-shadow:0 0 7px -1px currentColor; }
  #status { color:var(--mut); font-size:11px; }
  /* Layout */
  main { padding:20px 22px 40px; display:grid; gap:16px; max-width:960px; margin:0 auto; }
  .card { background:var(--panel); border:1px solid var(--line); border-radius:var(--radius); padding:16px 18px; }
  /* Stat strip */
  .stats { display:grid; grid-template-columns:repeat(4,1fr); gap:12px; }
  .stat { background:var(--panel); border:1px solid var(--line); border-radius:var(--radius); padding:12px 14px; min-width:0; }
  .stat-k { color:var(--mut); font-size:10.5px; text-transform:uppercase; letter-spacing:.07em; margin-bottom:5px; }
  .stat-v { font-size:18px; font-weight:600; color:var(--fg); overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .stat-v small { font-size:12px; color:var(--mut); font-weight:400; }
  .stat-v.mono { font-size:13px; font-weight:500; }
  .stat-copy { cursor:pointer; color:var(--acc); border-bottom:1px dotted var(--acc); }
  /* Inputs + buttons */
  input { background:var(--panel2); border:1px solid var(--line); color:var(--fg); padding:7px 9px; border-radius:var(--radius-sm); font:inherit; outline:none; transition:border-color .12s,box-shadow .12s; }
  input:focus { border-color:var(--acc); box-shadow:0 0 0 3px rgba(95,176,255,.13); }
  input::placeholder { color:var(--faint); }
  button { background:var(--acc); border:0; color:#06121f; padding:7px 13px; border-radius:var(--radius-sm); font:inherit; font-weight:600; cursor:pointer; transition:filter .12s,transform .04s; }
  button:hover { filter:brightness(1.08); }
  button:active { transform:translateY(1px); }
  button.ghost { background:transparent; color:var(--mut); border:1px solid var(--line); font-weight:500; }
  button.ghost:hover { color:var(--fg); border-color:var(--faint); filter:none; }
  button.danger:hover { color:var(--bad); border-color:var(--bad); }
  /* Tables */
  table { width:100%; border-collapse:collapse; margin-top:12px; }
  th,td { text-align:left; padding:8px 9px; border-bottom:1px solid var(--line); font-size:12.5px; vertical-align:middle; }
  th { color:var(--faint); font-weight:600; text-transform:uppercase; letter-spacing:.05em; font-size:10.5px; }
  tbody tr { transition:background .1s; }
  tbody tr:hover { background:rgba(255,255,255,.018); }
  tbody tr:last-child td { border-bottom:0; }
  td.empty { color:var(--faint); text-align:center; padding:18px 9px; }
  .row { display:flex; gap:8px; flex-wrap:wrap; align-items:center; }
  code { color:var(--acc); word-break:break-all; }
  .muted { color:var(--mut); }
  /* Connect block */
  .kv { display:flex; gap:10px; padding:3px 0; align-items:baseline; }
  .kv b { color:var(--mut); min-width:120px; font-weight:500; flex:0 0 auto; }
  .cmd { margin:10px 0; }
  .cmdlabel { color:var(--mut); font-size:11.5px; margin-bottom:5px; }
  .cmdrow { display:flex; gap:8px; align-items:flex-start; background:var(--panel2); border:1px solid var(--line); border-radius:var(--radius-sm); padding:9px 11px; }
  .cmdrow code { flex:1; white-space:pre-wrap; }
  .cmdrow button { flex:0 0 auto; }
  /* Setup stepper */
  .steps { display:flex; flex-wrap:wrap; gap:10px 18px; color:var(--mut); font-size:12px; margin:2px 0 2px; }
  .steps span { display:inline-flex; align-items:center; gap:7px; }
  .steps i { font-style:normal; width:18px; height:18px; border-radius:50%; border:1px solid var(--line); color:var(--acc); display:inline-flex; align-items:center; justify-content:center; font-size:11px; }
  .steps b { color:var(--fg); font-weight:600; }
  @media (max-width:680px){ .stats { grid-template-columns:repeat(2,1fr); } }
</style>
</head>
<body>
<header>
  <span class="brand"><span class="mark"></span><h1>warren <b>admin</b></h1></span>
  <span class="ver" id="ver"></span>
  <span class="spacer"></span>
  <span id="live" class="live">live</span>
  <button class="ghost" onclick="loadAll()">Refresh</button>
  <span id="status"></span>
</header>
<main>
  <div class="stats">
    <div class="stat"><div class="stat-k">Nodes online</div><div class="stat-v" id="stat-nodes">-</div></div>
    <div class="stat"><div class="stat-k">Traffic relayed</div><div class="stat-v" id="stat-traffic">-</div></div>
    <div class="stat"><div class="stat-k">Proxy endpoint</div><div class="stat-v mono" id="stat-proxy">-</div></div>
    <div class="stat"><div class="stat-k">Proxy users</div><div class="stat-v" id="stat-users">-</div></div>
  </div>

  <section class="card">
    <h2>Connect</h2>
    <div class="steps">
      <span><i>1</i> add a <b>proxy user</b></span>
      <span><i>2</i> create a <b>token</b>, run it on a device</span>
      <span><i>3</i> copy a command below</span>
    </div>
    <p class="desc" style="margin-top:10px">Your hub's addresses and ready-to-run commands. The fingerprint is the hub's public ID (safe to share); devices pin it to confirm they reached you.</p>
    <div id="connect">loading...</div>
  </section>

  <section class="card">
    <h2>Proxy users</h2>
    <p class="desc">Logins your apps put in curl / browser / scraper to send traffic through the pool. Not the admin token you used to open this page. A <code>+</code> is reserved for picking one device (<code>user+device</code>).</p>
    <div class="row">
      <input id="uname" placeholder="username">
      <input id="upass" type="password" placeholder="password">
      <button onclick="addUser()">Add user</button>
    </div>
    <table><thead><tr><th>Username</th><th>Traffic</th><th></th></tr></thead><tbody id="users"></tbody></table>
  </section>

  <section class="card">
    <h2>Enrollment tokens</h2>
    <p class="desc">A secret that lets a new device join automatically (no manual approval). Delete it to cut off devices still holding it.</p>
    <details class="hint"><summary>How do I rotate it?</summary><div class="hintbody">On the hub, run <code>warren enroll --name device</code> to mint a fresh token. Hand it out, then delete the old token here to cut off any device that still has the old one.</div></details>
    <div class="row">
      <input id="tname" placeholder="node name">
      <button onclick="addToken()">Create token</button>
    </div>
    <table><thead><tr><th>Name</th><th>Token</th><th>Created</th><th></th></tr></thead><tbody id="tokens"></tbody></table>
  </section>

  <section class="card">
    <h2>Live nodes</h2>
    <p class="desc">Devices connected and ready to carry requests. <b>Health</b> tracks recent dial errors (deprioritized at 3/3). <b>Success</b> is the dial success rate. Copy a device's command to route only through it.</p>
    <table><thead><tr><th>Node</th><th>Exit IP</th><th>Location</th><th>Up since</th><th>Health</th><th>Success</th><th>Traffic</th><th>Version</th><th></th></tr></thead><tbody id="nodes"></tbody></table>
  </section>

  <section class="card" id="card-pending">
    <h2>Pending approval</h2>
    <p class="desc">Devices that connected without a token. Match the short code against the device, then approve or deny.</p>
    <table><thead><tr><th>Code</th><th>Name</th><th>Key</th><th>First seen</th><th></th></tr></thead><tbody id="pending"></tbody></table>
  </section>

  <section class="card" id="card-keys">
    <h2>Approved keys</h2>
    <p class="desc">Device identities the hub trusts (each device made its own key on first run). Revoke one to remove that device for good.</p>
    <table><thead><tr><th>Name</th><th>Key</th><th>Approved</th><th></th></tr></thead><tbody id="keys"></tbody></table>
  </section>
  <section class="card">
    <h2>Hub log</h2>
    <p class="desc">Recent hub activity: enrollments, disconnects (with the reason a node dropped), and errors. Newest at the bottom. This is the hub's own view, not a node's internal crash reason.</p>
    <div class="row" style="margin-bottom:8px">
      <button class="ghost logf on" onclick="setLogFilter(this,'all')">All</button>
      <button class="ghost logf" onclick="setLogFilter(this,'warn')">Warn+</button>
      <button class="ghost logf" onclick="setLogFilter(this,'error')">Errors</button>
      <span class="spacer"></span>
      <button class="ghost" onclick="copyLogs(this)">copy shown</button>
    </div>
    <pre class="log" id="hublog"></pre>
  </section>
  <footer class="foot">Prometheus metrics at <code>GET /metrics</code> (same admin login, for scraping into Grafana).</footer>
</main>
<script>
let INFO = {};
function setStatus(m){ document.getElementById('status').textContent = m; }
async function api(method, path, body){
  // Page is behind HTTP Basic auth; the browser attaches credentials to these
  // same-origin requests automatically.
  const opt = { method, headers: {} };
  if (body){ opt.headers['Content-Type']='application/json'; opt.body = JSON.stringify(body); }
  const r = await fetch(path, opt);
  if (r.status === 401){ setStatus('unauthorized (reload to sign in)'); throw new Error('401'); }
  if (!r.ok){ setStatus('error ' + r.status); throw new Error(r.status); }
  setStatus('updated ' + new Date().toLocaleTimeString());
  const t = await r.text();
  return t ? JSON.parse(t) : null;
}
// Escapes for both HTML text and single/double-quoted attribute contexts, so a
// device-supplied node name cannot break out of an inline onclick handler.
function esc(s){ return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])); }
function fmtDate(s){ return s ? new Date(s*1000).toLocaleString() : ''; }
function fmtAgo(s){ if(!s) return ''; let d=Math.max(0,Date.now()/1000-s); if(d<60)return Math.floor(d)+'s'; if(d<3600)return Math.floor(d/60)+'m'; if(d<86400)return Math.floor(d/3600)+'h'; return Math.floor(d/86400)+'d'; }
function fmtBytes(n){ n = n||0; if (n < 1024) return n + ' B'; const u=['KB','MB','GB','TB']; let i=-1; do { n/=1024; i++; } while (n >= 1024 && i < u.length-1); return n.toFixed(1) + ' ' + u[i]; }
function shortKey(pk){ return pk && pk.length > 16 ? pk.slice(0,16)+'...' : (pk||''); }
function flash(btn, label){ const o=btn.textContent; btn.textContent=label||'copied'; setTimeout(()=>btn.textContent=o,1200); }
function copyEl(btn){
  const c = btn.parentElement.querySelector('code').textContent;
  navigator.clipboard.writeText(c).then(()=>flash(btn));
}
// Copy an explicit string (table rows where the text is not a sibling <code>).
function copyVal(btn, t){ navigator.clipboard.writeText(t).then(()=>flash(btn)); setStatus('copied'); }
function copyText(t){ navigator.clipboard.writeText(t); setStatus('copied'); }
function kv(k,v){ return `<div class="kv"><b>${k}</b><code>${esc(v)}</code></div>`; }
function cmd(label, c){ return `<div class="cmd"><div class="cmdlabel">${label}</div><div class="cmdrow"><code>${esc(c)}</code><button class="ghost" onclick="copyEl(this)">copy</button></div></div>`; }
function tlsFlags(){ return (INFO.tls && INFO.fingerprint) ? ` --tls --hub-fingerprint ${INFO.fingerprint}` : ''; }
function installUrl(){ return INFO.install_url||'https://raw.githubusercontent.com/doedja/warren/main/install.sh'; }
function installCmd(token){
  const node = INFO.node_addr || '<hub-host>:7000';
  return `curl -fsSL ${installUrl()} | sh -s -- --hub ${node} --token ${token}${tlsFlags()}`;
}
// One-paste installs using a join code (carries host + token + TLS + fingerprint).
function joinInstallCmd(code){ return `curl -fsSL ${installUrl()} | sh -s -- --join ${code}`; }
function winInstallCmd(code){ const ps = installUrl().replace('install.sh','install.ps1'); return `& ([scriptblock]::Create((irm ${ps}))) -Join ${code}`; }
async function loadInfo(){
  const i = await api('GET','/api/info'); INFO = i;
  const node = i.node_addr || '<hub-host>:7000';
  const proxy = i.proxy_addr || '<hub-host>:18080';
  const puser = i.proxy_user || 'USER';
  const ppass = i.proxy_pass || '<PASSWORD>';
  document.getElementById('ver').textContent = i.version ? 'v'+i.version : '';
  // Proxy endpoint stat (click to copy).
  const sp = document.getElementById('stat-proxy');
  sp.innerHTML = `<span class="stat-copy" title="click to copy">${esc(proxy)}</span>`;
  sp.querySelector('.stat-copy').onclick = ()=>copyText(proxy);
  let html = '';
  if (!i.proxy_user) html += `<p class="desc" style="color:var(--acc)">No proxy user yet. Add one under Proxy users below, or the proxy commands will not authenticate.</p>`;
  html += kv('Node link', node) + kv('Proxy', proxy);
  if (i.fingerprint) html += kv('Fingerprint (hub ID)', i.fingerprint);
  html += `<p class="desc" style="margin-top:10px">To add a device, use a <b>copy install</b> button on a token below (one paste, no flags). The manual form:</p>`;
  html += cmd('Add a node (manual)', `warren node run --hub ${node} --token <ENROLL_TOKEN>${tlsFlags()}`);
  html += cmd('Use the pool (HTTPS / CONNECT, auto-picks a device)', `curl -x http://${puser}:${ppass}@${proxy} https://api.ipify.org`);
  html += cmd('Use the pool (SOCKS5)', `curl -x socks5h://${puser}:${ppass}@${proxy} https://api.ipify.org`);
  html += cmd('Use ONE device (name after +, see Live nodes)', `curl -x http://${puser}+DEVICE:${ppass}@${proxy} https://api.ipify.org`);
  document.getElementById('connect').innerHTML = html;
}
async function loadPending(){
  const rows = await api('GET','/api/pending');
  document.getElementById('pending').innerHTML = rows.map(p =>
    `<tr><td><code>${esc(p.code)}</code></td><td>${esc(p.name)}</td><td class="muted">${esc(shortKey(p.pubkey))}</td><td class="muted">${fmtDate(p.first_seen)}</td>`+
    `<td><button onclick="approve('${esc(p.pubkey)}')">approve</button> `+
    `<button class="ghost danger" onclick="denyNode('${esc(p.pubkey)}')">deny</button></td></tr>`).join('')
    || '<tr><td class="empty" colspan=5>Nothing waiting. Devices that join with a token appear under Live nodes directly.</td></tr>';
  // Hide the card entirely when nothing is pending (keeps the dashboard lean).
  document.getElementById('card-pending').style.display = rows.length ? '' : 'none';
}
async function approve(pk){ await api('POST','/api/pending/'+encodeURIComponent(pk)+'/approve'); loadPending(); loadKeys(); }
async function denyNode(pk){ await api('DELETE','/api/pending/'+encodeURIComponent(pk)); loadPending(); }
async function loadNodes(){
  const rows = await api('GET','/api/nodes');
  const puser = INFO.proxy_user || 'USER';
  const ppass = INFO.proxy_pass || '<PASSWORD>';
  const proxy = INFO.proxy_addr || '<hub-host>:18080';
  document.getElementById('nodes').innerHTML = rows.map(n => {
    const c = `curl -x http://${puser}+${n.name}:${ppass}@${proxy} https://api.ipify.org`;
    const dotc = n.fails === 0 ? 'ok' : n.fails >= 3 ? 'bad' : 'warn';
    const lat = n.latency_ms != null ? ` <span class="muted">${n.latency_ms}ms</span>` : '';
    const fcol = n.fails >= 3 ? 'color:var(--bad)' : '';
    const fail = `<span class="dot ${dotc}"></span><span style="${fcol}">${n.fails} / 3</span>${lat}`;
    const ip = n.ip ? `<code>${esc(n.ip)}</code>` : '<span class="muted">pending</span>';
    const loc = [n.city, n.country].filter(Boolean).map(esc).join(', ') || '<span class="muted">-</span>';
    let succ;
    if (!n.dials) {
      succ = '<span class="muted">-</span>';
    } else {
      const pct = Math.round(n.success_rate);
      const col = pct >= 95 ? 'var(--mut)' : 'var(--bad)';
      const err = n.last_error ? ` title="last error: ${esc(n.last_error)}"` : '';
      succ = `<span style="color:${col}"${err}>${pct}% <span class="muted">(${n.dials})</span></span>`;
    }
    const up = `<span title="${fmtDate(n.since)}">${fmtAgo(n.since)}</span>`;
    const ver = n.version ? `<code>v${esc(n.version)}</code>` : '<span class="muted">-</span>';
    return `<tr><td><b>${esc(n.id)}</b></td><td>${ip}</td><td>${loc}</td><td>${up}</td><td>${fail}</td><td>${succ}</td><td>${fmtBytes(n.bytes)}</td><td>${ver}</td>`+
      `<td><button class="ghost" onclick='copyVal(this, ${esc(JSON.stringify(c))})'>copy cmd</button></td></tr>`;
  }).join('') || '<tr><td class="empty" colspan=9>No devices online yet. Create a token below and run the install line on a device.</td></tr>';
  // Stat strip: online count + total relayed traffic.
  document.getElementById('stat-nodes').innerHTML = rows.length + ' <small>online</small>';
  const total = rows.reduce((a,n)=>a+(n.bytes||0),0);
  document.getElementById('stat-traffic').textContent = fmtBytes(total);
}
async function loadKeys(){
  const rows = await api('GET','/api/node-keys');
  document.getElementById('keys').innerHTML = rows.map(k =>
    `<tr><td>${esc(k.name)}</td><td><code>${esc(shortKey(k.pubkey))}</code></td><td class="muted">${fmtDate(k.approved_at)}</td>`+
    `<td><button class="ghost danger" onclick="revokeKey('${esc(k.pubkey)}')">revoke</button></td></tr>`).join('')
    || '<tr><td class="empty" colspan=4>none</td></tr>';
  document.getElementById('card-keys').style.display = rows.length ? '' : 'none';
}
async function revokeKey(pk){ await api('DELETE','/api/node-keys/'+encodeURIComponent(pk)); loadKeys(); }
let LOGS = [];
let LOG_FILTER = 'all';
function logMatch(l){ return LOG_FILTER === 'all' ? true : LOG_FILTER === 'warn' ? / (WARN|ERROR) /.test(l) : / ERROR /.test(l); }
function renderLogs(){
  const el = document.getElementById('hublog');
  // Keep the view pinned to the newest line unless the user scrolled up to read.
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  const lines = LOGS.filter(logMatch);
  el.innerHTML = lines.map(l => {
    const cls = / ERROR /.test(l) ? 'le' : / WARN /.test(l) ? 'lw' : '';
    return `<span class="${cls}">${esc(l)}</span>`;
  }).join('\n') || '<span class="muted">no matching lines</span>';
  if (atBottom) el.scrollTop = el.scrollHeight;
}
function setLogFilter(btn, f){
  LOG_FILTER = f;
  document.querySelectorAll('.logf').forEach(b => b.classList.toggle('on', b === btn));
  renderLogs();
}
// Copy exactly what is shown (respects the active filter), as plain text.
function copyLogs(btn){ copyVal(btn, LOGS.filter(logMatch).join('\n')); }
async function loadLogs(){ LOGS = await api('GET','/api/logs'); renderLogs(); }
async function loadTokens(){
  const rows = await api('GET','/api/tokens');
  document.getElementById('tokens').innerHTML = rows.map(t => {
    // With a join code, offer a copy button per OS; otherwise the manual command.
    let copy;
    if (t.join_code) {
      copy = `<button class="ghost" onclick='copyVal(this, ${esc(JSON.stringify(joinInstallCmd(t.join_code)))})'>copy (Linux/mac)</button> `+
             `<button class="ghost" onclick='copyVal(this, ${esc(JSON.stringify(winInstallCmd(t.join_code)))})'>copy (Windows)</button> `;
    } else {
      copy = `<button class="ghost" onclick='copyVal(this, ${esc(JSON.stringify(installCmd(t.token)))})'>copy install</button> `;
    }
    return `<tr><td>${esc(t.name)}</td><td><code>${esc(t.token)}</code></td><td class="muted">${fmtDate(t.created)}</td>`+
    `<td>${copy}<button class="ghost danger" onclick="delToken('${esc(t.token)}')">delete</button></td></tr>`;
  }).join('')
    || '<tr><td class="empty" colspan=4>No tokens yet. Create one to let a device join automatically.</td></tr>';
}
async function addToken(){
  const name = document.getElementById('tname').value.trim() || 'node';
  const res = await api('POST','/api/tokens',{name});
  if (res && res.token) setStatus('token: ' + res.token);
  document.getElementById('tname').value=''; loadTokens();
}
async function delToken(t){ await api('DELETE','/api/tokens/'+encodeURIComponent(t)); loadTokens(); }
async function loadUsers(){
  const rows = await api('GET','/api/users');
  document.getElementById('users').innerHTML = rows.map(u =>
    `<tr><td><b>${esc(u.username)}</b></td><td>${fmtBytes(u.bytes)}</td><td><button class="ghost danger" onclick="delUser('${esc(u.username)}')">delete</button></td></tr>`).join('')
    || '<tr><td class="empty" colspan=3>No proxy users yet. Add one so apps can authenticate to the proxy.</td></tr>';
  document.getElementById('stat-users').textContent = rows.length;
}
async function addUser(){
  const username = document.getElementById('uname').value.trim();
  const password = document.getElementById('upass').value;
  if (!username || !password){ setStatus('username and password are both required'); return; }
  if (username.includes('+')){ setStatus("username cannot contain '+' (reserved for device selection)"); return; }
  await api('POST','/api/users',{username,password});
  document.getElementById('uname').value=''; document.getElementById('upass').value=''; loadUsers();
}
async function delUser(u){ await api('DELETE','/api/users/'+encodeURIComponent(u)); loadUsers(); }
function setLive(ok){ const el=document.getElementById('live'); if(!el) return; el.textContent = ok?'live':'stale'; el.style.color = ok?'var(--ok)':'var(--mut)'; }
async function loadAll(){
  try { await loadInfo(); await loadPending(); await loadNodes(); await loadKeys(); await loadTokens(); await loadUsers(); await loadLogs(); setLive(true); }
  catch(e){ setLive(false); }
}
loadAll();
setInterval(loadAll, 5000);
</script>
</body>
</html>
"###;
