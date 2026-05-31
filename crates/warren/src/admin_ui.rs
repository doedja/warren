//! Static HTML for the hub admin dashboard, served at `/`.

pub const DASHBOARD: &str = r###"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>warren admin</title>
<style>
  :root { --bg:#0d0f12; --card:#161a20; --line:#262c36; --fg:#e6e9ef; --mut:#8a93a3; --acc:#5fa8ff; }
  * { box-sizing:border-box; }
  body { margin:0; background:var(--bg); color:var(--fg); font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace; }
  header { padding:16px 20px; border-bottom:1px solid var(--line); display:flex; gap:10px; align-items:center; flex-wrap:wrap; }
  h1 { font-size:16px; margin:0 12px 0 0; }
  h2 { font-size:13px; color:var(--mut); margin:0 0 10px; text-transform:uppercase; letter-spacing:.05em; }
  main { padding:20px; display:grid; gap:18px; max-width:900px; }
  .card { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:16px; }
  input { background:#0b0d10; border:1px solid var(--line); color:var(--fg); padding:6px 8px; border-radius:5px; font:inherit; }
  button { background:var(--acc); border:0; color:#06121f; padding:6px 12px; border-radius:5px; font:inherit; cursor:pointer; }
  button.ghost { background:transparent; color:var(--mut); border:1px solid var(--line); }
  table { width:100%; border-collapse:collapse; margin-top:10px; }
  th,td { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); font-size:13px; }
  th { color:var(--mut); font-weight:600; }
  .row { display:flex; gap:8px; flex-wrap:wrap; align-items:center; }
  code { color:var(--acc); word-break:break-all; }
  #status { color:var(--mut); margin-left:auto; }
</style>
</head>
<body>
<header>
  <h1>warren admin</h1>
  <input id="tok" type="password" placeholder="admin token" style="min-width:220px">
  <button onclick="saveTok()">Save</button>
  <button class="ghost" onclick="loadAll()">Refresh</button>
  <span id="status"></span>
</header>
<main>
  <section class="card">
    <h2>Nodes</h2>
    <table><thead><tr><th>Node</th><th>Fails</th></tr></thead><tbody id="nodes"></tbody></table>
  </section>
  <section class="card">
    <h2>Enrollment tokens</h2>
    <div class="row">
      <input id="tname" placeholder="node name">
      <button onclick="addToken()">Create token</button>
    </div>
    <table><thead><tr><th>Name</th><th>Token</th><th>Created</th><th></th></tr></thead><tbody id="tokens"></tbody></table>
  </section>
  <section class="card">
    <h2>Proxy users</h2>
    <div class="row">
      <input id="uname" placeholder="username">
      <input id="upass" type="password" placeholder="password">
      <button onclick="addUser()">Add user</button>
    </div>
    <table><thead><tr><th>Username</th><th></th></tr></thead><tbody id="users"></tbody></table>
  </section>
</main>
<script>
let TOK = localStorage.getItem('warren_admin_token') || '';
document.getElementById('tok').value = TOK;
function saveTok(){ TOK = document.getElementById('tok').value.trim(); localStorage.setItem('warren_admin_token', TOK); loadAll(); }
function setStatus(m){ document.getElementById('status').textContent = m; }
async function api(method, path, body){
  const opt = { method, headers: { 'Authorization': 'Bearer ' + TOK } };
  if (body){ opt.headers['Content-Type']='application/json'; opt.body = JSON.stringify(body); }
  const r = await fetch(path, opt);
  if (r.status === 401){ setStatus('unauthorized'); throw new Error('401'); }
  if (!r.ok){ setStatus('error ' + r.status); throw new Error(r.status); }
  setStatus('ok');
  const t = await r.text();
  return t ? JSON.parse(t) : null;
}
function esc(s){ return String(s).replace(/[&<>]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;'}[c])); }
function fmtDate(s){ return s ? new Date(s*1000).toISOString().slice(0,19).replace('T',' ') : ''; }
async function loadNodes(){
  const rows = await api('GET','/api/nodes');
  document.getElementById('nodes').innerHTML = rows.map(n =>
    `<tr><td>${esc(n.id)}</td><td>${n.fails}</td></tr>`).join('') || '<tr><td colspan=2>no nodes</td></tr>';
}
async function loadTokens(){
  const rows = await api('GET','/api/tokens');
  document.getElementById('tokens').innerHTML = rows.map(t =>
    `<tr><td>${esc(t.name)}</td><td><code>${esc(t.token)}</code></td><td>${fmtDate(t.created)}</td>`+
    `<td><button class="ghost" onclick="delToken('${esc(t.token)}')">delete</button></td></tr>`).join('')
    || '<tr><td colspan=4>no tokens</td></tr>';
}
async function loadUsers(){
  const rows = await api('GET','/api/users');
  document.getElementById('users').innerHTML = rows.map(u =>
    `<tr><td>${esc(u)}</td><td><button class="ghost" onclick="delUser('${esc(u)}')">delete</button></td></tr>`).join('')
    || '<tr><td colspan=2>no users</td></tr>';
}
async function addToken(){
  const name = document.getElementById('tname').value.trim() || 'node';
  const res = await api('POST','/api/tokens',{name});
  if (res && res.token) setStatus('token: ' + res.token);
  document.getElementById('tname').value=''; loadTokens();
}
async function delToken(t){ await api('DELETE','/api/tokens/'+encodeURIComponent(t)); loadTokens(); }
async function addUser(){
  const username = document.getElementById('uname').value.trim();
  const password = document.getElementById('upass').value;
  if (!username) return;
  await api('POST','/api/users',{username,password});
  document.getElementById('uname').value=''; document.getElementById('upass').value=''; loadUsers();
}
async function delUser(u){ await api('DELETE','/api/users/'+encodeURIComponent(u)); loadUsers(); }
async function loadAll(){ try { await loadNodes(); await loadTokens(); await loadUsers(); } catch(e){} }
if (TOK) loadAll();
</script>
</body>
</html>
"###;
