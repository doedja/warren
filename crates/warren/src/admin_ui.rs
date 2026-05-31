//! Static HTML for the hub admin dashboard, served at `/` (behind Basic auth).

pub const DASHBOARD: &str = r###"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>warren admin</title>
<style>
  :root { --bg:#0d0f12; --card:#161a20; --line:#262c36; --fg:#e6e9ef; --mut:#8a93a3; --acc:#5fa8ff; --ok:#46c46e; }
  * { box-sizing:border-box; }
  body { margin:0; background:var(--bg); color:var(--fg); font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace; }
  header { padding:16px 20px; border-bottom:1px solid var(--line); display:flex; gap:12px; align-items:center; flex-wrap:wrap; }
  h1 { font-size:16px; margin:0 8px 0 0; }
  h2 { font-size:13px; color:var(--mut); margin:0 0 4px; text-transform:uppercase; letter-spacing:.05em; }
  .desc { color:var(--mut); font-size:12px; margin:0 0 12px; line-height:1.45; max-width:62ch; }
  .hint { color:var(--acc); cursor:help; border-bottom:1px dotted var(--acc); }
  main { padding:20px; display:grid; gap:18px; max-width:920px; }
  .card { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:16px; }
  input { background:#0b0d10; border:1px solid var(--line); color:var(--fg); padding:6px 8px; border-radius:5px; font:inherit; }
  button { background:var(--acc); border:0; color:#06121f; padding:6px 12px; border-radius:5px; font:inherit; cursor:pointer; }
  button.ghost { background:transparent; color:var(--mut); border:1px solid var(--line); }
  table { width:100%; border-collapse:collapse; margin-top:10px; }
  th,td { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); font-size:13px; vertical-align:top; }
  th { color:var(--mut); font-weight:600; }
  .row { display:flex; gap:8px; flex-wrap:wrap; align-items:center; }
  code { color:var(--acc); word-break:break-all; }
  #status { color:var(--mut); margin-left:auto; }
  .live { color:var(--ok); font-size:11px; border:1px solid var(--line); border-radius:10px; padding:1px 8px; }
  .kv { display:flex; gap:10px; padding:3px 0; }
  .kv b { color:var(--mut); min-width:110px; font-weight:600; }
  .cmd { margin:10px 0; }
  .cmdlabel { color:var(--mut); font-size:12px; margin-bottom:4px; }
  .cmdrow { display:flex; gap:8px; align-items:flex-start; background:#0b0d10; border:1px solid var(--line); border-radius:6px; padding:8px 10px; }
  .cmdrow code { flex:1; white-space:pre-wrap; }
  .cmdrow button { flex:0 0 auto; }
</style>
</head>
<body>
<header>
  <h1>warren admin</h1>
  <span id="live" class="live">live</span>
  <button class="ghost" onclick="loadAll()">Refresh</button>
  <span id="status"></span>
</header>
<main>
  <p class="desc" style="margin:0 0 4px">New here? Three steps: add a <b>proxy user</b> (a login for your apps), create an <b>enrollment token</b> and run it on a device, then use a command from <b>Connect</b>. The cards below follow that order.</p>
  <section class="card">
    <h2>Connect</h2>
    <p class="desc">Your hub's addresses, plus ready-to-run commands. Copy one, fill in a token or password, run it on a device or client. The fingerprint is your hub's public ID: devices pin it to be sure they reached you, so it is safe to share.</p>
    <div id="connect">loading...</div>
  </section>
  <section class="card">
    <h2>Proxy users</h2>
    <p class="desc">Logins for apps that send traffic through the pool: the username and password you put in curl, your browser, or a scraper. Note: this is not the password you typed to open this dashboard (that one is the admin token). A `+` is not allowed in a username; it is reserved for picking one device (user+device).</p>
    <div class="row">
      <input id="uname" placeholder="username">
      <input id="upass" type="password" placeholder="password">
      <button onclick="addUser()">Add user</button>
    </div>
    <table><thead><tr><th>Username</th><th></th></tr></thead><tbody id="users"></tbody></table>
  </section>
  <section class="card">
    <h2>Enrollment tokens</h2>
    <p class="desc">A secret that lets a new device join automatically (no manual approval). Hand it to a device with --token. Delete it to stop new devices joining with it. <span class="hint" title="On the hub, run:  warren enroll --name device&#10;That mints a fresh token. Hand it out, then delete the old token here to cut off any device that still has the old one.">How do I rotate it?</span></p>
    <div class="row">
      <input id="tname" placeholder="node name">
      <button onclick="addToken()">Create token</button>
    </div>
    <table><thead><tr><th>Name</th><th>Token</th><th>Created</th><th></th></tr></thead><tbody id="tokens"></tbody></table>
  </section>
  <section class="card">
    <h2>Live nodes</h2>
    <p class="desc">Devices connected right now and ready to carry requests. Up since is when each connected. Fails counts recent dial errors; the hub deprioritizes a device once it reaches 3. Copy a device's command to send traffic out only through that one device.</p>
    <table><thead><tr><th>Node</th><th>Up since</th><th>Fails</th><th>Use just this device</th></tr></thead><tbody id="nodes"></tbody></table>
  </section>
  <section class="card">
    <h2>Pending approval</h2>
    <p class="desc">Devices that connected without an enrollment token. Approve one to let it serve traffic, or deny it. Match the short code against the device to be sure it is yours.</p>
    <table><thead><tr><th>Code</th><th>Name</th><th>Key</th><th>First seen</th><th></th></tr></thead><tbody id="pending"></tbody></table>
  </section>
  <section class="card">
    <h2>Approved keys</h2>
    <p class="desc">Device identities the hub trusts (each device made its own key on first run). Revoke one to kick that device out of the pool for good.</p>
    <table><thead><tr><th>Name</th><th>Key</th><th>Approved</th><th></th></tr></thead><tbody id="keys"></tbody></table>
  </section>
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
function shortKey(pk){ return pk && pk.length > 16 ? pk.slice(0,16)+'...' : (pk||''); }
function copyEl(btn){
  const c = btn.parentElement.querySelector('code').textContent;
  navigator.clipboard.writeText(c).then(()=>{ btn.textContent='copied'; setTimeout(()=>btn.textContent='copy',1200); });
}
function kv(k,v){ return `<div class="kv"><b>${k}</b><code>${esc(v)}</code></div>`; }
function cmd(label, c){ return `<div class="cmd"><div class="cmdlabel">${label}</div><div class="cmdrow"><code>${esc(c)}</code><button class="ghost" onclick="copyEl(this)">copy</button></div></div>`; }
function tlsFlags(){ return (INFO.tls && INFO.fingerprint) ? ` --tls --hub-fingerprint ${INFO.fingerprint}` : ''; }
function installCmd(token){
  const node = INFO.node_addr || '<hub-host>:7000';
  return `curl -fsSL ${INFO.install_url||'https://raw.githubusercontent.com/doedja/warren/main/install.sh'} | sh -s -- --hub ${node} --token ${token}${tlsFlags()}`;
}
async function loadInfo(){
  const i = await api('GET','/api/info'); INFO = i;
  const node = i.node_addr || '<hub-host>:7000';
  const proxy = i.proxy_addr || '<hub-host>:18080';
  const puser = i.proxy_user || 'USER';
  let html = '';
  if (!i.proxy_user) html += `<p class="desc" style="color:var(--acc)">No proxy user yet. Add one under Proxy users below first, or the proxy commands will not authenticate.</p>`;
  html += kv('Node link', node) + kv('Proxy', proxy);
  if (i.fingerprint) html += kv('Fingerprint (hub ID)', i.fingerprint);
  html += cmd('Add a node (installer, fill in a token from below)', installCmd('<ENROLL_TOKEN>'));
  html += cmd('Add a node (existing binary)', `warren node run --hub ${node} --token <ENROLL_TOKEN>${tlsFlags()}`);
  html += cmd('Use the pool (HTTPS / CONNECT, auto-picks a device)', `curl -x http://${puser}:<PASSWORD>@${proxy} https://api.ipify.org`);
  html += cmd('Use the pool (SOCKS5)', `curl -x socks5h://${puser}:<PASSWORD>@${proxy} https://api.ipify.org`);
  html += cmd('Use ONE device (put its name after +, see Live nodes)', `curl -x http://${puser}+DEVICE:<PASSWORD>@${proxy} https://api.ipify.org`);
  document.getElementById('connect').innerHTML = html;
}
async function loadPending(){
  const rows = await api('GET','/api/pending');
  document.getElementById('pending').innerHTML = rows.map(p =>
    `<tr><td><code>${esc(p.code)}</code></td><td>${esc(p.name)}</td><td>${esc(shortKey(p.pubkey))}</td><td>${fmtDate(p.first_seen)}</td>`+
    `<td><button onclick="approve('${esc(p.pubkey)}')">approve</button> `+
    `<button class="ghost" onclick="denyNode('${esc(p.pubkey)}')">deny</button></td></tr>`).join('')
    || '<tr><td colspan=5>Nothing waiting. Devices that join with a token appear under Live nodes directly.</td></tr>';
}
async function approve(pk){ await api('POST','/api/pending/'+encodeURIComponent(pk)+'/approve'); loadPending(); loadKeys(); }
async function denyNode(pk){ await api('DELETE','/api/pending/'+encodeURIComponent(pk)); loadPending(); }
async function loadNodes(){
  const rows = await api('GET','/api/nodes');
  const puser = INFO.proxy_user || 'USER';
  const proxy = INFO.proxy_addr || '<hub-host>:18080';
  document.getElementById('nodes').innerHTML = rows.map(n => {
    const c = `curl -x http://${puser}+${n.name}:<PASSWORD>@${proxy} https://api.ipify.org`;
    const fail = n.fails >= 3 ? `<span style="color:#e0564b">${n.fails} / 3</span>` : `${n.fails} / 3`;
    return `<tr><td>${esc(n.id)}</td><td>${fmtDate(n.since)}</td><td>${fail}</td>`+
      `<td><button class="ghost" onclick='copyText(${esc(JSON.stringify(c))})'>copy proxy cmd</button></td></tr>`;
  }).join('') || '<tr><td colspan=4>No devices online yet. Create a token below and run the install line on a device.</td></tr>';
}
function copyText(t){ navigator.clipboard.writeText(t); setStatus('proxy command copied'); }
async function loadKeys(){
  const rows = await api('GET','/api/node-keys');
  document.getElementById('keys').innerHTML = rows.map(k =>
    `<tr><td>${esc(k.name)}</td><td><code>${esc(shortKey(k.pubkey))}</code></td><td>${fmtDate(k.approved_at)}</td>`+
    `<td><button class="ghost" onclick="revokeKey('${esc(k.pubkey)}')">revoke</button></td></tr>`).join('')
    || '<tr><td colspan=4>none</td></tr>';
}
async function revokeKey(pk){ await api('DELETE','/api/node-keys/'+encodeURIComponent(pk)); loadKeys(); }
async function loadTokens(){
  const rows = await api('GET','/api/tokens');
  document.getElementById('tokens').innerHTML = rows.map(t =>
    `<tr><td>${esc(t.name)}</td><td><code>${esc(t.token)}</code></td><td>${fmtDate(t.created)}</td>`+
    `<td><button class="ghost" onclick='copyInstall(${esc(JSON.stringify(t.token))})'>copy install</button> `+
    `<button class="ghost" onclick="delToken('${esc(t.token)}')">delete</button></td></tr>`).join('')
    || '<tr><td colspan=4>No tokens yet. Create one to let a device join automatically.</td></tr>';
}
function copyInstall(token){ navigator.clipboard.writeText(installCmd(token)); setStatus('install command copied for token'); }
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
    `<tr><td>${esc(u)}</td><td><button class="ghost" onclick="delUser('${esc(u)}')">delete</button></td></tr>`).join('')
    || '<tr><td colspan=2>No proxy users yet. Add one so apps can authenticate to the proxy.</td></tr>';
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
  try { await loadInfo(); await loadPending(); await loadNodes(); await loadKeys(); await loadTokens(); await loadUsers(); setLive(true); }
  catch(e){ setLive(false); }
}
loadAll();
setInterval(loadAll, 5000);
</script>
</body>
</html>
"###;
