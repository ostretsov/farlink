// Farlink web page: the administrator sees everything; a person sees only
// their own computers.
let token = localStorage.getItem('farlink_token') || '';
let data = null;
let grantMachine = null;
const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

async function api(method, path, body) {
  const r = await fetch(path, {
    method,
    headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + token },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (r.status === 401) { showLogin(); throw new Error('signed out'); }
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || r.statusText);
  return j;
}

function toast(text) {
  const t = document.createElement('div');
  t.className = 'toast'; t.textContent = text;
  document.body.appendChild(t); setTimeout(() => t.remove(), 2200);
}
function copyText(s) { navigator.clipboard.writeText(s); toast('Copied'); }

function showLogin() {
  $('login').classList.remove('hidden'); $('dash').classList.add('hidden'); $('logout').classList.add('hidden');
  $('me').textContent = '';
}
async function login(e) {
  e.preventDefault();
  try {
    const body = JSON.stringify({ username: $('user').value, password: $('pw').value });
    const r = await fetch('/api/login', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body });
    const j = await r.json();
    if (!r.ok) { $('loginErr').textContent = j.error || 'Could not sign in'; return; }
    token = j.token; localStorage.setItem('farlink_token', token);
    $('pw').value = '';
    $('login').classList.add('hidden'); refresh();
  } catch (err) { $('loginErr').textContent = String(err); }
}
async function logout() {
  await api('POST', '/api/logout').catch(() => {});
  token = ''; localStorage.removeItem('farlink_token'); showLogin();
}

function ago(secs) {
  if (secs < 60) return 'just now';
  if (secs < 3600) return Math.floor(secs / 60) + ' min ago';
  if (secs < 86400) return Math.floor(secs / 3600) + ' h ago';
  return Math.floor(secs / 86400) + ' d ago';
}
function left(secs) {
  if (secs <= 0) return 'ended';
  if (secs < 3600) return 'in ' + Math.ceil(secs / 60) + ' min';
  if (secs < 86400) return 'in ' + Math.round(secs / 3600) + ' h';
  return 'in ' + Math.round(secs / 86400) + ' d';
}
function screens(mons) {
  if (!mons || !mons.length) return '<span class="muted">–</span>';
  return '<span class="screens">' + mons.map((m) => `<i title="${esc(m.width + '×' + m.height)}" style="width:${Math.round(14 * m.width / m.height)}px"></i>`).join('') + '</span> ' + mons.length;
}

function ownerSelect(m) {
  const opts = ['<option value="">Administrator only</option>']
    .concat(data.users.map((u) => `<option value="${u.id}" ${m.owner === u.id ? 'selected' : ''}>${esc(u.name)}</option>`));
  return `<select onchange="setOwner('${m.id}', this.value)">${opts.join('')}</select>`;
}

function render() {
  if (!data) return;
  const admin = data.role === 'admin';
  document.querySelectorAll('.admin-only').forEach((el) => el.classList.toggle('hidden', !admin));
  $('me').textContent = admin ? 'Administrator' : data.user_name;
  $('mtitle').textContent = admin ? 'Computers' : 'My computers';
  $('ver').textContent = 'server ' + data.server_version + (admin && data.release_version ? ' · published ' + data.release_version : '');
  $('installUrl').textContent = data.install_url;
  const q = $('filter').value.toLowerCase();
  const ms = data.machines.filter((m) => !q || (m.name + m.hostname + m.os + (m.owner_name || '')).toLowerCase().includes(q));
  $('mcount').textContent = data.machines.filter((m) => m.online).length + ' online of ' + data.machines.length;
  $('machines').innerHTML = ms.map((m) => `<tr ${admin ? `oncontextmenu="rowMenu(event, '${m.id}')" title="Right-click to rename"` : ''}>
    <td><span class="dot ${m.online ? 'on' : ''}"></span><b>${esc(m.name)}</b>
      ${m.sessions ? '<span class="tag ctrl">in use</span>' : ''}
      <div class="muted">${m.online ? 'online' : 'seen ' + ago(data.now - m.last_seen)} · ${esc(m.hostname)}</div></td>
    <td>${esc(m.os)}</td><td>${screens(m.monitors)}</td>
    ${admin ? `<td>${ownerSelect(m)}</td>` : ''}
    <td>${esc(m.version)} ${m.up_to_date ? '' : '<span class="tag view">updating</span>'}</td>
    <td style="text-align:right;white-space:nowrap">
      <a class="btn small primary" href="/view/${m.id}" target="_blank" ${m.online ? '' : 'style="opacity:.5"'}>Control</a>
      <a class="btn small" href="/view/${m.id}?view_only=1" target="_blank">View</a>
      ${admin ? `<button class="small" onclick="openGrant('${m.id}')">Temporary link</button>
      <button class="small" onclick="rename('${m.id}')">Rename</button>
      <button class="small danger" onclick="removeMachine('${m.id}')">Remove</button>` : ''}</td></tr>`).join('')
    || `<tr><td colspan="6" class="muted">${admin ? 'No computers yet. Open the install link above on a computer to add it.' : 'No computers yet. Ask the administrator for your personal install link.'}</td></tr>`;

  if (!admin) return;

  const byId = (id) => data.machines.find((m) => m.id === id);
  $('users').innerHTML = data.users.map((u) => {
    const pcs = u.machines.map(byId).filter(Boolean);
    const installed = pcs.length
      ? pcs.map((m) => `<span class="dot ${m.online ? 'on' : ''}"></span>${esc(m.name)} <span class="muted">installed</span>`).join('<br>')
      : '<span class="tag view">waiting for install</span>';
    return `<tr><td><b>${esc(u.name)}</b></td><td class="mono">${esc(u.login)}</td><td>${installed}</td>
      <td><button class="small" onclick="copyText('${u.install_url}')">Copy link</button></td>
      <td style="text-align:right;white-space:nowrap"><button class="small" onclick="resetPw('${u.id}')">New password</button>
      <button class="small danger" onclick="removeUser('${u.id}')">Remove</button></td></tr>`;
  }).join('') || '<tr><td colspan="5" class="muted">Nobody yet. “Add person” gives someone access to their own computer only.</td></tr>';

  const gs = data.grants.filter((g) => g.status === 'active').concat(data.grants.filter((g) => g.status !== 'active').slice(0, 5));
  $('grants').innerHTML = gs.map((g) => `<tr>
    <td>${esc(g.label || '—')}</td><td>${esc(g.machine_name)}</td>
    <td>${g.view_only ? '<span class="tag view">view only</span>' : '<span class="tag ctrl">control</span>'}</td>
    <td>${g.status === 'active' ? left(g.expires - data.now) : '<span class="tag bad">' + g.status + '</span>'}</td>
    <td style="text-align:right;white-space:nowrap">${g.status === 'active'
      ? `<button class="small" onclick="copyText('${g.url}')">Copy</button> <button class="small danger" onclick="revoke('${g.token}')">Withdraw</button>` : ''}</td></tr>`).join('')
    || '<tr><td colspan="5" class="muted">No links. Use “Temporary link” next to a computer.</td></tr>';

  const name = (id) => (byId(id) || {}).name || id;
  $('sessions').innerHTML = data.sessions.map((s) => `<tr><td>${esc(s.who)}</td><td>${esc(name(s.machine_id))}</td>
    <td>${s.view_only ? '<span class="tag view">view</span>' : '<span class="tag ctrl">control</span>'}</td>
    <td style="text-align:right"><button class="small danger" onclick="endSession('${s.id}')">End</button></td></tr>`).join('')
    || '<tr><td colspan="4" class="muted">Nobody is connected.</td></tr>';
}

async function refresh() {
  if (!token) { showLogin(); return; }
  if (document.activeElement && document.activeElement.tagName === 'SELECT') return; // don't close an open menu
  try {
    data = await api('GET', '/api/overview');
    $('dash').classList.remove('hidden'); $('logout').classList.remove('hidden'); $('login').classList.add('hidden');
    render();
  } catch (e) { /* shown by api() */ }
}

function openGrant(id) {
  grantMachine = id;
  $('gName').textContent = data.machines.find((m) => m.id === id).name;
  $('gResult').classList.add('hidden'); $('gLabel').value = '';
  $('grantDlg').showModal();
}
async function createGrant(e) {
  e.preventDefault();
  try {
    const g = await api('POST', '/api/grants', { machine_id: grantMachine, minutes: +$('gMinutes').value, view_only: $('gMode').value === 'view', label: $('gLabel').value });
    $('gUrl').textContent = g.url; $('gResult').classList.remove('hidden'); refresh();
  } catch (err) { toast(err.message); }
}
function openUser() {
  ['uName', 'uLogin', 'uPw'].forEach((id) => { $(id).value = ''; });
  $('uResult').classList.add('hidden');
  $('userDlg').showModal();
}
async function createUser(e) {
  e.preventDefault();
  try {
    const u = await api('POST', '/api/users', { name: $('uName').value, login: $('uLogin').value, password: $('uPw').value });
    $('uUrl').textContent = u.install_url; $('uResult').classList.remove('hidden'); refresh();
  } catch (err) { toast(err.message); }
}
async function resetPw(id) {
  const pw = prompt('New password (at least 8 characters)');
  if (pw) { try { await api('POST', '/api/users/' + id + '/password', { password: pw }); toast('Password changed'); } catch (e) { toast(e.message); } }
}
async function removeUser(id) {
  const u = data.users.find((x) => x.id === id);
  if (confirm('Remove ' + u.name + '? Their computers stay, visible to the administrator only.')) {
    await api('DELETE', '/api/users/' + id); refresh();
  }
}
async function setOwner(machine, userId) {
  try { await api('POST', '/api/machines/' + machine + '/owner', { user_id: userId || null }); toast('Saved'); } catch (e) { toast(e.message); }
  document.activeElement.blur(); refresh();
}
async function revoke(t) { await api('DELETE', '/api/grants/' + t); toast('Access withdrawn'); refresh(); }
async function endSession(id) { await api('POST', '/api/sessions/' + id + '/end'); refresh(); }
async function rename(id) {
  closeMenu();
  const m = data.machines.find((x) => x.id === id);
  const n = prompt('New name for this computer', m.name);
  if (n && n.trim() && n.trim() !== m.name) {
    try { await api('POST', '/api/machines/' + id + '/rename', { name: n.trim() }); toast('Renamed'); refresh(); } catch (e) { toast(e.message); }
  }
}
// Right-click menu on a computer row (administrator only).
function closeMenu() { document.querySelectorAll('.ctxmenu').forEach((el) => el.remove()); }
function rowMenu(ev, id) {
  ev.preventDefault();
  closeMenu();
  const menu = document.createElement('div');
  menu.className = 'ctxmenu';
  menu.style.left = ev.clientX + 'px';
  menu.style.top = ev.clientY + 'px';
  const item = document.createElement('button');
  item.textContent = 'Rename…';
  item.onclick = () => rename(id);
  menu.append(item);
  document.body.append(menu);
}
document.addEventListener('click', (e) => { if (!e.target.closest('.ctxmenu')) closeMenu(); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeMenu(); });
async function removeMachine(id) {
  const m = data.machines.find((x) => x.id === id);
  if (confirm('Remove ' + m.name + '? Its links stop working. The computer must be installed again to come back.')) {
    await api('DELETE', '/api/machines/' + id); refresh();
  }
}
async function rotate() {
  if (confirm('Create a new general install link? The old one will stop working (installed computers are not affected).')) {
    await api('POST', '/api/enroll/rotate'); refresh();
  }
}

refresh();
setInterval(refresh, 5000);
