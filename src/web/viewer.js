// Farlink browser viewer: draws remote screens on canvases and sends input.
(() => {
  const cfg = window.FARLINK;
  const $ = (id) => document.getElementById(id);
  const params = new URLSearchParams(location.search);
  let viewOnly = cfg.mode === 'guest' ? !!cfg.view_only : params.get('view_only') === '1';
  const token = localStorage.getItem('farlink_token') || '';
  const PRESETS = {
    fast: { scale_pct: 60, max_fps: 12, codec: 'jpeg', jpeg_quality: 35 },
    balanced: { scale_pct: 85, max_fps: 20, codec: 'jpeg', jpeg_quality: 65 },
    sharp: { scale_pct: 100, max_fps: 25, codec: 'png', jpeg_quality: 85 },
  };
  const KEYS = {
    Enter: 'Enter', NumpadEnter: 'Enter', Escape: 'Escape', Backspace: 'Backspace', Tab: 'Tab', Space: 'Space',
    Insert: 'Insert', Delete: 'Delete', Home: 'Home', End: 'End', PageUp: 'PageUp', PageDown: 'PageDown',
    ArrowLeft: 'ArrowLeft', ArrowRight: 'ArrowRight', ArrowUp: 'ArrowUp', ArrowDown: 'ArrowDown',
    ShiftLeft: 'Shift', ShiftRight: 'Shift', ControlLeft: 'Control', ControlRight: 'Control', AltLeft: 'Alt', AltRight: 'Alt',
    MetaLeft: 'Meta', MetaRight: 'Meta', CapsLock: 'CapsLock', Minus: 'Minus', Equal: 'Equals', BracketLeft: 'OpenBracket',
    BracketRight: 'CloseBracket', Backslash: 'Backslash', Semicolon: 'Semicolon', Quote: 'Quote', Comma: 'Comma',
    Period: 'Period', Slash: 'Slash', Backquote: 'Backtick', PrintScreen: 'PrintScreen', Pause: 'Pause',
    NumLock: 'NumLock', ScrollLock: 'ScrollLock', ContextMenu: 'ContextMenu',
  };
  const keyName = (code) => KEYS[code] || (/^Key[A-Z]$/.test(code) ? code.slice(3) : /^Digit\d$/.test(code) ? code.slice(5) : /^F\d{1,2}$/.test(code) ? code : null);

  const showName = (name) => {
    $('title').textContent = name || 'Farlink';
    document.title = (name || 'Remote') + ' · Farlink';
  };
  showName(cfg.name);
  // Keep the computer's name in sync when the administrator renames it.
  setInterval(async () => {
    try {
      if (cfg.mode === 'guest') {
        const r = await fetch('/api/guest/' + cfg.token);
        if (r.ok) showName((await r.json()).name);
      } else {
        const r = await fetch('/api/overview', { headers: { Authorization: 'Bearer ' + localStorage.getItem('farlink_token') } });
        const m = r.ok && (await r.json()).machines.find((x) => x.id === cfg.machine);
        if (m) showName(m.name);
      }
    } catch (e) { /* offline for a moment: keep the old name */ }
  }, 10000);
  // View-only switch during the session (not for view-only temporary links).
  let viewLocked = cfg.mode === 'guest' && !!cfg.view_only;
  const setMode = () => {
    $('mode').textContent = viewLocked ? 'View only (link)' : viewOnly ? 'View only: on' : 'View only: off';
    $('mode').className = 'tag ' + (viewOnly ? 'view' : 'ctrl') + (viewLocked ? '' : ' switch');
    $('mode').title = viewLocked ? 'This link only allows watching' : 'Click to switch between watching and control';
    $('cad').disabled = viewOnly;
  };
  $('mode').onclick = () => {
    if (viewLocked) return;
    viewOnly = !viewOnly; setMode();
    send({ t: 'set_view_only', view_only: viewOnly });
  };
  setMode();

  const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
  const url = cfg.mode === 'guest'
    ? `${proto}${location.host}/ws/guest/${cfg.token}`
    : `${proto}${location.host}/ws/view/${cfg.machine}?token=${encodeURIComponent(token)}&view_only=${viewOnly ? 1 : 0}`;
  const ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  const send = (m) => ws.readyState === 1 && ws.send(JSON.stringify(m));
  const input = (event) => { if (!viewOnly) send({ t: 'input', event }); };

  const mons = new Map(); // index -> {canvas, ctx, wrap}
  let monitors = [];
  let chain = Promise.resolve();
  let bytes = 0, frames = 0;

  function buildScreens(list) {
    monitors = list;
    $('stage').innerHTML = '';
    mons.clear();
    const sel = $('screens');
    sel.innerHTML = '<option value="all">All screens</option>' + list.map((m) => `<option value="${m.index}">Screen ${m.index + 1}</option>`).join('');
    for (const m of list) {
      const wrap = document.createElement('div');
      wrap.className = 'mon';
      const canvas = document.createElement('canvas');
      canvas.width = m.width; canvas.height = m.height; canvas.tabIndex = 0;
      const label = document.createElement('div');
      label.className = 'label'; label.textContent = `Screen ${m.index + 1} · ${m.width}×${m.height}`;
      wrap.append(canvas, label);
      $('stage').append(wrap);
      mons.set(m.index, { canvas, ctx: canvas.getContext('2d'), wrap });
      hookInput(canvas, m.index);
    }
    showScreens();
  }
  function showScreens() {
    const v = $('screens').value;
    for (const [i, m] of mons) m.wrap.classList.toggle('hidden', v !== 'all' && +v !== i);
  }
  // Only the chosen screens are captured on the remote computer.
  $('screens').onchange = () => {
    showScreens();
    const v = $('screens').value;
    send({ t: 'watch', monitors: v === 'all' ? monitors.map((m) => m.index) : [+v] });
  };
  document.addEventListener('visibilitychange', () => {
    // Browser tab hidden: stop pictures entirely until it is shown again.
    if (document.hidden) send({ t: 'watch', monitors: [] });
    else $('screens').onchange();
  });

  function sendQuality() {
    const q = Object.assign({}, PRESETS[$('quality').value], { color: $('color').value });
    send({ t: 'quality', quality: q });
  }
  $('quality').onchange = sendQuality;
  $('color').onchange = sendQuality;
  $('cad').onclick = () => input({ k: 'ctrl_alt_del' });
  $('fs').onclick = () => (document.fullscreenElement ? document.exitFullscreen() : document.documentElement.requestFullscreen());

  function pos(canvas, e) {
    const r = canvas.getBoundingClientRect();
    return { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height };
  }
  function hookInput(canvas, mon) {
    let pending = null;
    canvas.addEventListener('mousemove', (e) => {
      const first = !pending;
      pending = pos(canvas, e);
      if (first) requestAnimationFrame(() => { input({ k: 'mouse_move', mon, x: pending.x, y: pending.y }); pending = null; });
    });
    const btn = (b) => (b === 0 ? 'left' : b === 1 ? 'middle' : 'right');
    canvas.addEventListener('mousedown', (e) => { canvas.focus(); const p = pos(canvas, e); input({ k: 'mouse_button', mon, x: p.x, y: p.y, button: btn(e.button), down: true }); e.preventDefault(); });
    canvas.addEventListener('mouseup', (e) => { const p = pos(canvas, e); input({ k: 'mouse_button', mon, x: p.x, y: p.y, button: btn(e.button), down: false }); e.preventDefault(); });
    canvas.addEventListener('contextmenu', (e) => e.preventDefault());
    canvas.addEventListener('wheel', (e) => {
      e.preventDefault();
      const unit = e.deltaMode === 1 ? 1 : 1 / 100;
      input({ k: 'scroll', dx: -e.deltaX * unit, dy: -e.deltaY * unit });
    }, { passive: false });
    canvas.addEventListener('keydown', (e) => {
      e.preventDefault();
      const plain = e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey;
      if (plain) { input({ k: 'text', text: e.key }); return; }
      const k = keyName(e.code);
      if (k) input({ k: 'key', key: k, down: true });
    });
    canvas.addEventListener('keyup', (e) => {
      e.preventDefault();
      const plain = e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey;
      const k = keyName(e.code);
      if (k && !plain) input({ k: 'key', key: k, down: false });
    });
  }

  ws.onopen = () => { $('status').textContent = 'Waiting for the computer…'; };
  ws.onmessage = (e) => {
    if (typeof e.data === 'string') {
      const m = JSON.parse(e.data);
      if (m.t === 'hello') {
        $('overlay')?.remove();
        if (m.view_only) viewOnly = true; setMode();
        $('status').textContent = `${m.os} · ${m.monitors.length} screen(s)`;
        buildScreens(m.monitors);
        sendQuality();
      } else if (m.t === 'view_mode') {
        viewOnly = m.view_only; viewLocked = m.locked; setMode();
      } else if (m.t === 'monitors') {
        buildScreens(m.monitors); send({ t: 'refresh' });
      } else if (m.t === 'notice') {
        $('status').textContent = m.text;
      } else if (m.t.startsWith('fs_')) {
        fileMessage(m);
      } else if (m.t === 'closed') {
        $('stage').innerHTML = `<div class="overlay"><h2>Disconnected</h2><div>${m.reason.replace(/</g, '&lt;')}</div><button onclick="location.reload()">Connect again</button></div>`;
        $('status').textContent = 'Disconnected';
      }
      return;
    }
    const buf = e.data;
    bytes += buf.byteLength;
    const dv = new DataView(buf);
    if (buf.byteLength >= 6 && dv.getUint8(0) === 0x46) { fileChunk(dv, buf); return; }
    if (buf.byteLength < 20 || dv.getUint8(0) !== 0x54) return;
    const mon = dv.getUint8(1), fmt = dv.getUint8(2), flags = dv.getUint8(3);
    const fw = dv.getUint16(4, true), fh = dv.getUint16(6, true);
    const x = dv.getUint16(8, true), y = dv.getUint16(10, true);
    const seq = dv.getUint32(16, true);
    const blob = new Blob([new Uint8Array(buf, 20)], { type: fmt === 0 ? 'image/jpeg' : 'image/png' });
    // Draw in arrival order so older tiles never cover newer ones.
    chain = chain.then(async () => {
      const m = mons.get(mon);
      if (!m) return;
      if (m.canvas.width !== fw || m.canvas.height !== fh) { m.canvas.width = fw; m.canvas.height = fh; }
      const bmp = await createImageBitmap(blob);
      m.ctx.drawImage(bmp, x, y); bmp.close();
      if (flags & 1) { frames++; send({ t: 'ack', seq }); }
    }).catch(() => {});
  };
  ws.onclose = () => { if ($('status').textContent !== 'Disconnected') $('status').textContent = 'Connection lost'; };

  // ---- files (control sessions only) ----
  let fsReq = 0;
  let fsDir = { path: '', parent: null };
  const downloads = new Map(); // req -> { name, size, parts, got }
  const fmtSize = (n) => n < 1024 ? n + ' B' : n < 1048576 ? (n / 1024).toFixed(1) + ' KB' : n < 1073741824 ? (n / 1048576).toFixed(1) + ' MB' : (n / 1073741824).toFixed(1) + ' GB';
  const fsStatus = (t) => { $('fsTransfers').textContent = t; };
  const fsList = (path) => send({ t: 'fs_list', req: ++fsReq, path });
  $('filesBtn').onclick = () => { $('files').classList.toggle('hidden'); if (!$('files').classList.contains('hidden')) fsList(fsDir.path); };
  $('fsClose').onclick = () => $('files').classList.add('hidden');
  $('fsUp').onclick = () => { if (fsDir.parent !== null) fsList(fsDir.parent); };

  function fileMessage(m) {
    if (m.t === 'fs_listing') {
      fsDir = m;
      $('fsPath').textContent = m.path || 'Starting places';
      $('fsUp').disabled = m.parent === null;
      const list = $('fsList');
      list.innerHTML = '';
      if (m.error) list.textContent = m.error;
      for (const e of m.entries) {
        const row = document.createElement('div');
        row.className = e.is_dir ? 'dir' : '';
        row.innerHTML = `<span></span><span class="size">${e.is_dir ? '' : fmtSize(e.size)}</span>`;
        row.firstChild.textContent = (e.is_dir ? '▸ ' : '') + e.name;
        row.ondblclick = () => {
          if (e.is_dir) fsList(e.path);
          else { const req = ++fsReq; send({ t: 'fs_download', req, path: e.path }); fsStatus('Starting download of ' + e.name + '…'); }
        };
        list.append(row);
      }
    } else if (m.t === 'fs_begin') {
      downloads.set(m.req, { name: m.name, size: m.size, parts: [], got: 0 });
      if (m.size === 0) finishDownload(m.req);
    } else if (m.t === 'fs_done') {
      downloads.delete(m.req);
      fsStatus(m.error ? 'Failed: ' + m.error : 'Done: ' + m.path);
      fsList(fsDir.path);
    }
  }
  function finishDownload(req) {
    const d = downloads.get(req);
    downloads.delete(req);
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob(d.parts));
    a.download = d.name; a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 10000);
    fsStatus('Downloaded ' + d.name + ' (' + fmtSize(d.got) + ')');
  }
  function fileChunk(dv, buf) {
    const req = dv.getUint32(1, true), last = dv.getUint8(5) & 1;
    const d = downloads.get(req);
    if (!d) return;
    d.parts.push(new Uint8Array(buf.slice(6)));
    d.got += buf.byteLength - 6;
    fsStatus('Downloading ' + d.name + ': ' + fmtSize(d.got) + ' of ' + fmtSize(d.size));
    if (last) finishDownload(req);
  }
  $('fsUpload').onchange = async (ev) => {
    const file = ev.target.files[0];
    ev.target.value = '';
    if (!file || !fsDir.path) { fsStatus('Open a folder first.'); return; }
    const req = ++fsReq, CHUNK = 256 * 1024;
    send({ t: 'fs_upload', req, dir: fsDir.path, name: file.name, size: file.size });
    for (let off = 0; off < file.size || (off === 0 && file.size === 0); off += CHUNK) {
      if (file.size === 0) break;
      while (ws.bufferedAmount > 4 * CHUNK) await new Promise((r) => setTimeout(r, 20));
      const data = new Uint8Array(await file.slice(off, off + CHUNK).arrayBuffer());
      const msg = new Uint8Array(6 + data.length);
      const hdr = new DataView(msg.buffer);
      hdr.setUint8(0, 0x46); hdr.setUint32(1, req, true); hdr.setUint8(5, off + CHUNK >= file.size ? 1 : 0);
      msg.set(data, 6);
      ws.send(msg);
      fsStatus('Uploading ' + file.name + ': ' + fmtSize(Math.min(off + CHUNK, file.size)) + ' of ' + fmtSize(file.size));
    }
  };
  const hideFilesIfViewOnly = () => { $('filesBtn').classList.toggle('hidden', viewOnly); };
  hideFilesIfViewOnly();
  setInterval(hideFilesIfViewOnly, 1000);

  setInterval(() => {
    if (monitors.length && ws.readyState === 1) {
      $('status').textContent = `${monitors.length} screen(s) · ${frames} fps · ${Math.round(bytes / 1024)} KB/s` + (cfg.mode === 'guest' ? ` · link ends ${new Date(cfg.expires * 1000).toLocaleTimeString()}` : '');
    }
    bytes = 0; frames = 0;
  }, 1000);
})();
