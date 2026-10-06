'use strict';
/* share — browser UI. No framework, no external requests, no innerHTML with server data. */

const $ = (sel) => document.querySelector(sel);
const SVG_NS = 'http://www.w3.org/2000/svg';
const PAGE = 200;
const BASE = (location.pathname.match(/^(\/s\/[^/]+)/) || [''])[0];

const state = {
  path: '', q: '', sort: 'name', order: 'asc', recursive: false,
  entries: [], total: 0, truncated: false, loading: false, gen: 0, status: null,
};

// ---------- helpers ----------
function icon(name) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('class', 'i');
  svg.setAttribute('aria-hidden', 'true');
  const use = document.createElementNS(SVG_NS, 'use');
  use.setAttribute('href', '#i-' + name);
  svg.appendChild(use);
  return svg;
}

function el(tag, props = {}, ...kids) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'class') node.className = v;
    else if (k === 'text') node.textContent = v;
    else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) node.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids) if (kid != null) node.append(kid);
  return node;
}

const encPath = (p) => p.split('/').filter(Boolean).map(encodeURIComponent).join('/');
const joinPath = (a, b) => [a, b].filter(Boolean).join('/');
const downloadUrl = (path, inline) => BASE + '/download/' + encPath(path) + (inline ? '?inline=1' : '');
const archiveUrl = (path, fmt = 'zip') =>
  BASE + '/archive' + (path ? '/' + encPath(path) : '') + '?format=' + encodeURIComponent(fmt);
const parentOf = (p) => p.split('/').slice(0, -1).join('/');

function closeAllMenus() {
  for (const m of document.querySelectorAll('.dl-menu:not([hidden])')) m.hidden = true;
}

function buildArchiveMenu(path, folderName) {
  const baseName = folderName || 'archive';
  const menu = el('div', { class: 'dl-menu', role: 'menu', hidden: true });
  for (const [fmt, label] of [['zip', '.zip'], ['tar.gz', '.tar.gz'], ['tar', '.tar']]) {
    const item = el('a', {
      class: 'dl-item',
      role: 'menuitem',
      href: archiveUrl(path, fmt),
      download: baseName + '.' + fmt,
      text: 'Download ' + label,
      onclick: () => { menu.hidden = true; },
    });
    menu.append(item);
  }
  const btn = el('button', {
    type: 'button',
    class: 'btn icon dl-toggle',
    title: 'Download folder (' + baseName + ')',
    'aria-label': 'Download folder ' + baseName,
    onclick: (ev) => {
      ev.stopPropagation();
      const wasHidden = menu.hidden;
      closeAllMenus();
      menu.hidden = !wasHidden;
    },
  }, icon('download'));
  return el('div', { class: 'dl-wrap' }, btn, menu);
}

function updateRootDownloadMenu() {
  const wrap = $('#root-dl');
  if (!wrap) return;
  if (!state.status || state.status.kind !== 'dir') {
    wrap.hidden = true;
    return;
  }
  const folderName = state.path ? state.path.split('/').pop() : (state.status.name || 'share');
  const built = buildArchiveMenu(state.path, folderName);
  wrap.replaceChildren(...built.childNodes);
  wrap.hidden = false;
}

function fmtBytes(n) {
  if (n < 1024) return n + ' B';
  const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let v = n, i = -1;
  do { v /= 1024; i++; } while (v >= 1024 && i < units.length - 1);
  return v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2) + ' ' + units[i];
}
function fmtRate(bps) {
  if (bps >= 1e9) return (bps / 1e9).toFixed(2) + ' GB/s';
  if (bps >= 1e6) return (bps / 1e6).toFixed(1) + ' MB/s';
  if (bps >= 1e3) return (bps / 1e3).toFixed(0) + ' kB/s';
  return Math.round(bps) + ' B/s';
}
const dateFmt = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
const fmtDate = (secs) => (secs == null ? '' : dateFmt.format(new Date(secs * 1000)));

let toastTimer = 0;
function toast(msg) {
  const t = $('#toast');
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 4500);
}

async function api(url, opts = {}) {
  const headers = Object.assign({ Accept: 'application/json', 'X-Requested-With': 'ShareUI' }, opts.headers || {});
  const res = await fetch(BASE + url, Object.assign({}, opts, { headers }));
  let body = null;
  try { body = await res.json(); } catch { /* not JSON */ }
  if (res.status === 401) {
    showAuthModal(body && body.auth_mode);
    throw new Error((body && body.error) || 'Authentication required');
  }
  if (!res.ok) throw new Error((body && body.error) || 'HTTP ' + res.status);
  return body;
}

const isTextOrCode = (e) =>
  e.kind === 'text' || e.kind === 'code' || e.mime.startsWith('text/');

const previewable = (e) =>
  !e.is_dir && (/^(image|video|audio)\//.test(e.mime) && e.mime !== 'image/svg+xml' ||
    e.mime === 'application/pdf' || isTextOrCode(e));

// ---------- auth modal ----------
function showAuthModal(mode) {
  const modal = $('#auth-modal');
  const needsUser = mode === 'user_pass';
  $('#auth-user-wrap').hidden = !needsUser;
  $('#auth-pass-label').textContent = needsUser ? 'Password' : 'PIN';
  $('#auth-desc').textContent = needsUser
    ? 'Enter your username and password to access this share.'
    : 'Enter the PIN to access this share.';
  $('#auth-err').hidden = true;
  modal.hidden = false;
  setTimeout(() => (needsUser ? $('#auth-user') : $('#auth-pass')).focus(), 20);
}

// ---------- media lightbox ----------
const lightbox = { list: [], index: -1 };

function openLightbox(entry) {
  lightbox.list = state.entries.filter(previewable);
  lightbox.index = lightbox.list.findIndex((e) => e.path === entry.path);
  if (lightbox.index < 0) {
    lightbox.list = [entry];
    lightbox.index = 0;
  }
  $('#lightbox').hidden = false;
  renderLightbox();
}

function closeLightbox() {
  const lb = $('#lightbox');
  if (lb.hidden) return;
  lb.hidden = true;
  $('#lb-body').replaceChildren();
  lightbox.list = [];
  lightbox.index = -1;
}

function stepLightbox(delta) {
  if (!lightbox.list.length) return;
  lightbox.index = (lightbox.index + delta + lightbox.list.length) % lightbox.list.length;
  renderLightbox();
}

async function renderLightbox() {
  const e = lightbox.list[lightbox.index];
  if (!e) return;
  const url = downloadUrl(e.path, true);
  $('#lb-title').textContent = e.name;
  $('#lb-pos').textContent = lightbox.list.length > 1 ? (lightbox.index + 1) + ' / ' + lightbox.list.length : '';
  const dl = $('#lb-dl');
  dl.href = downloadUrl(e.path, false);
  dl.setAttribute('download', e.name);
  $('#lb-prev').hidden = lightbox.list.length <= 1;
  $('#lb-next').hidden = lightbox.list.length <= 1;

  const body = $('#lb-body');
  body.replaceChildren();
  if (e.mime.startsWith('image/')) {
    body.append(el('img', { class: 'lb-media', src: url, alt: e.name }));
  } else if (e.mime.startsWith('video/')) {
    body.append(el('video', { class: 'lb-media', src: url, controls: true, autoplay: true }));
  } else if (e.mime.startsWith('audio/')) {
    body.append(el('audio', { class: 'lb-audio', src: url, controls: true, autoplay: true }));
  } else if (e.mime === 'application/pdf') {
    body.append(el('iframe', { class: 'lb-pdf', src: url, title: e.name }));
  } else if (isTextOrCode(e)) {
    const pre = el('pre', { class: 'lb-text', text: 'Loading…' });
    body.append(pre);
    try {
      const res = await fetch(url);
      pre.textContent = res.ok ? await res.text() : 'Could not load preview (HTTP ' + res.status + ')';
    } catch (err) {
      pre.textContent = 'Could not load preview: ' + err.message;
    }
  }
}

// ---------- routing ----------
function readLocation() {
  const stripped = BASE && location.pathname.startsWith(BASE)
    ? location.pathname.slice(BASE.length) || '/'
    : location.pathname;
  const rest = stripped.replace(/^\/browse\/?/, '');
  state.path = stripped.startsWith('/browse')
    ? rest.split('/').filter(Boolean).map(decodeURIComponent).join('/') : '';
  const p = new URLSearchParams(location.search);
  state.q = p.get('q') || '';
  state.sort = ['name', 'size', 'modified', 'type'].includes(p.get('sort')) ? p.get('sort') : 'name';
  state.order = p.get('order') === 'desc' ? 'desc' : 'asc';
  state.recursive = p.get('rec') === '1';
}

function urlFor(path) {
  const p = new URLSearchParams();
  if (state.q) p.set('q', state.q);
  if (state.sort !== 'name') p.set('sort', state.sort);
  if (state.order !== 'asc') p.set('order', state.order);
  if (state.recursive && state.q) p.set('rec', '1');
  const qs = p.toString();
  return BASE + '/browse' + (path ? '/' + encPath(path) : '') + (qs ? '?' + qs : '');
}

function navigate(path) {
  state.path = path;
  state.q = '';
  $('#q').value = '';
  history.pushState(null, '', urlFor(path));
  load();
  window.scrollTo({ top: 0 });
}

function internalLink(a, path) {
  a.href = urlFor(path);
  a.addEventListener('click', (ev) => {
    if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0) return;
    ev.preventDefault();
    navigate(path);
  });
  return a;
}

// ---------- rendering ----------
function renderCrumbs() {
  const nav = $('#crumbs');
  nav.replaceChildren();
  const parts = state.path ? state.path.split('/') : [];
  const root = el('a', { text: 'Home' });
  if (parts.length) nav.append(internalLink(root, ''));
  else nav.append(el('span', { class: 'here', text: 'Home' }));
  parts.forEach((part, i) => {
    nav.append(el('span', { class: 'sep', text: '/' }));
    const path = parts.slice(0, i + 1).join('/');
    if (i === parts.length - 1) nav.append(el('span', { class: 'here', text: part }));
    else nav.append(internalLink(el('a', { text: part }), path));
  });
}

function buildRow(e) {
  const name = el('a', { class: 'name' }, el('span', { class: 'nm', text: e.name }));
  if (e.is_dir) {
    internalLink(name, e.path);
  } else {
    name.href = downloadUrl(e.path, previewable(e));
    if (previewable(e)) {
      name.target = '_blank';
      name.rel = 'noopener';
      name.addEventListener('click', (ev) => {
        if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0) return;
        ev.preventDefault();
        openLightbox(e);
      });
    }
  }
  const where = parentOf(e.path);
  if (where && state.recursive && state.q) name.append(el('span', { class: 'where', text: where }));

  const bar = el('i', { class: 'bar' });
  const acts = el('span', { class: 'acts' });
  if (previewable(e)) {
    const prevBtn = el('a', {
      class: 'btn icon',
      href: downloadUrl(e.path, true),
      target: '_blank',
      rel: 'noopener',
      title: 'Preview',
      'aria-label': 'Preview ' + e.name,
    }, icon('eye'));
    prevBtn.addEventListener('click', (ev) => {
      if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || ev.button !== 0) return;
      ev.preventDefault();
      openLightbox(e);
    });
    acts.append(prevBtn);
  }
  if (e.is_dir) {
    acts.append(buildArchiveMenu(e.path, e.name));
  } else {
    acts.append(el('a', {
      class: 'btn icon',
      href: downloadUrl(e.path, false),
      download: e.name,
      title: 'Download',
      'aria-label': 'Download ' + e.name,
    }, icon('download')));
  }
  const row = el('li', { class: 'row k-' + e.kind },
    el('span', { class: 'ico' }, icon(e.kind)),
    name,
    el('span', { class: 'meta' },
      el('span', { class: 'size' }, e.is_dir ? '' : fmtBytes(e.size), bar),
      el('time', { class: 'mod', text: fmtDate(e.modified) })),
    acts);
  row._bar = bar;
  row._size = e.is_dir ? 0 : e.size;
  return row;
}

function updateBars() {
  const rows = [...$('#list').children];
  const max = rows.reduce((m, r) => Math.max(m, r._size || 0), 0);
  for (const r of rows) r._bar.style.setProperty('--w', max ? ((r._size / max) * 100).toFixed(1) + '%' : '0%');
}

function renderSummary() {
  const s = $('#summary');
  if (state.loading && !state.entries.length) { s.textContent = 'Loading…'; return; }
  const n = state.total;
  let text = n === 1 ? '1 item' : n.toLocaleString() + ' items';
  if (state.q) text += ' matching “' + state.q + '”';
  if (state.entries.length < n) text += ' · showing ' + state.entries.length.toLocaleString();
  if (state.truncated) text += ' · search stopped at its limit, refine the filter';
  s.textContent = text;
}

function showEmpty(msg, strong) {
  const e = $('#empty');
  e.replaceChildren();
  if (strong) e.append(el('b', { text: strong }), document.createElement('br'));
  e.append(document.createTextNode(msg));
  e.hidden = false;
}

function syncUploadChrome(enabled) {
  if (!state.status) state.status = { kind: 'dir', upload_enabled: Boolean(enabled) };
  state.status.upload_enabled = Boolean(enabled);
  const chip = $('#chip');
  chip.hidden = false;
  const secure = location.protocol === 'https:';
  chip.textContent = (secure ? 'Encrypted' : 'Not encrypted') + ' · ' + (enabled ? 'uploads on' : 'read-only');
  chip.classList.toggle('warn', !secure);
  const btn = $('#upload-btn');
  btn.hidden = !enabled;
  if (enabled && !btn.childNodes.length) {
    btn.replaceChildren(icon('upload'), document.createTextNode('Upload'));
  }
}

// ---------- loading ----------
async function load({ reset = true } = {}) {
  if (reset) {
    state.gen++;
    state.entries = [];
    state.total = 0;
    $('#list').replaceChildren();
    $('#empty').hidden = true;
    renderCrumbs();
    updateRootDownloadMenu();
  } else if (state.loading || state.entries.length >= state.total) {
    return;
  }
  const gen = state.gen;
  state.loading = true;
  renderSummary();
  const p = new URLSearchParams({
    path: state.path, sort: state.sort, order: state.order,
    offset: String(state.entries.length), limit: String(PAGE),
  });
  if (state.q) p.set('q', state.q);
  if (state.recursive && state.q) p.set('recursive', 'true');
  try {
    const data = await api('/api/list?' + p);
    if (gen !== state.gen) return; // a newer request superseded this one
    state.total = data.total;
    state.truncated = data.truncated;
    if (typeof data.upload_enabled === 'boolean') syncUploadChrome(data.upload_enabled);
    $('#share-name').textContent = data.share_name;
    document.title = (state.path ? state.path.split('/').pop() + ' · ' : '') + data.share_name;
    updateRootDownloadMenu();
    const list = $('#list');
    for (const e of data.entries) { state.entries.push(e); list.append(buildRow(e)); }
    updateBars();
    if (!state.entries.length) {
      showEmpty(state.q ? 'Nothing matches that filter.' : 'This folder is empty.', state.q ? '' : '');
    }
  } catch (err) {
    if (gen !== state.gen) return;
    showEmpty(err.message, 'Could not load this folder');
  } finally {
    if (gen === state.gen) { state.loading = false; renderSummary(); }
  }
}

// ---------- status / chrome ----------
async function loadStatus() {
  try {
    const s = await api('/api/status');
    state.status = s;
    document.body.dataset.mode = s.kind;
    $('#share-name').textContent = s.name;
    syncUploadChrome(s.upload_enabled);
    $('#rec-wrap').hidden = !(s.recursive_search && s.kind === 'dir');
    updateRootDownloadMenu();
  } catch { /* the listing reports its own errors */ }
}

function applyTheme(theme) {
  if (theme) document.documentElement.dataset.theme = theme;
  const dark = document.documentElement.dataset.theme
    ? document.documentElement.dataset.theme === 'dark'
    : matchMedia('(prefers-color-scheme: dark)').matches;
  $('#theme').replaceChildren(icon(dark ? 'sun' : 'moon'));
  return dark;
}

// ---------- uploads (including recursive folders & resumable chunks) ----------
const uploads = { queue: [], running: 0, max: 3, seq: 0 };

function uploadTarget() { return state.status && state.status.upload_fixed_dir ? 'the upload folder' : (state.path || 'Home'); }

function enqueue(items) {
  if (!state.status || !state.status.upload_enabled) { toast('Uploads are disabled on this server.'); return; }
  const baseDir = state.path;
  for (const raw of items) {
    const file = raw.file || raw;
    const subDir = raw.subDir || '';
    const dir = joinPath(baseDir, subDir);
    const mkdir = Boolean(subDir);
    const label = subDir ? subDir + '/' + file.name : file.name;
    const item = {
      id: ++uploads.seq, file, dir, mkdir, offset: 0,
      xhr: null, start: 0, lastT: 0, lastB: 0, rate: 0,
    };
    item.fill = el('i');
    item.status = el('span', { text: 'Waiting…' });
    item.retry = el('button', {
      type: 'button',
      class: 'link',
      hidden: true,
      text: 'Resume',
      onclick: () => retryUpload(item),
    });
    item.cancel = el('button', {
      type: 'button',
      title: 'Cancel',
      'aria-label': 'Cancel upload of ' + label,
      onclick: () => cancelUpload(item),
    }, icon('x'));
    item.node = el('li', { class: 'up' },
      el('div', { class: 'line' }, el('span', { text: label }), el('span', { text: fmtBytes(file.size) })),
      el('div', { class: 'meter' }, item.fill),
      el('div', { class: 'sub' }, item.status, el('span', {}, item.retry, item.cancel)));
    $('#uploads-list').prepend(item.node);
    uploads.queue.push(item);
  }
  $('#uploads').hidden = false;
  pump();
}

function pump() {
  while (uploads.running < uploads.max && uploads.queue.length) startUpload(uploads.queue.shift());
}

function finishUpload(item, ok, message, canResume = false) {
  uploads.running = Math.max(0, uploads.running - 1);
  item.done = ok;
  item.node.classList.toggle('error', !ok);
  item.status.textContent = message;
  item.cancel.hidden = ok;
  item.retry.hidden = !canResume;
  if (ok) item.fill.style.setProperty('--p', '100%');
  if (ok && (item.dir === state.path || item.dir.startsWith(state.path ? state.path + '/' : ''))) {
    scheduleRefresh();
  }
  pump();
}

async function queryResumeOffset(item) {
  try {
    const p = new URLSearchParams({
      name: item.file.name,
      dir: item.dir,
      size: String(item.file.size),
      mtime: String(item.file.lastModified || 0),
    });
    if (item.mkdir) p.set('mkdir', 'true');
    const data = await api('/api/upload/status?' + p);
    const srvOffset = Number(data && data.offset);
    return Number.isFinite(srvOffset) && srvOffset > 0 && srvOffset <= item.file.size ? srvOffset : 0;
  } catch {
    return 0;
  }
}

async function retryUpload(item) {
  item.retry.hidden = true;
  item.node.classList.remove('error');
  item.status.textContent = 'Checking resume offset…';
  item.offset = await queryResumeOffset(item);
  uploads.queue.unshift(item);
  pump();
}

async function startUpload(item) {
  uploads.running++;
  if (item.offset === 0 && item.file.size > 0) {
    item.offset = await queryResumeOffset(item);
  }
  const xhr = new XMLHttpRequest();
  item.xhr = xhr;
  item.start = item.lastT = performance.now();
  item.lastB = 0;
  const p = new URLSearchParams({
    name: item.file.name,
    dir: item.dir,
    size: String(item.file.size),
    mtime: String(item.file.lastModified || 0),
  });
  if (item.mkdir) p.set('mkdir', 'true');
  if (item.offset > 0) p.set('offset', String(item.offset));
  xhr.open('POST', BASE + '/api/upload?' + p);
  xhr.setRequestHeader('X-Share-Upload', '1');
  xhr.setRequestHeader('X-Share-Offset', String(item.offset));
  xhr.upload.onprogress = (e) => {
    const now = performance.now();
    if (now - item.lastT >= 400) {
      item.rate = ((e.loaded - item.lastB) / (now - item.lastT)) * 1000;
      item.lastT = now; item.lastB = e.loaded;
    }
    const total = item.file.size || (e.lengthComputable ? item.offset + e.total : 0);
    const loaded = item.offset + e.loaded;
    const pct = total ? (loaded / total) * 100 : 0;
    item.fill.style.setProperty('--p', pct.toFixed(1) + '%');
    item.status.textContent = pct.toFixed(0) + '% · ' + fmtRate(item.rate);
  };
  xhr.onload = () => {
    let body = null;
    try { body = JSON.parse(xhr.responseText); } catch { /* ignore */ }
    if (xhr.status === 201) {
      finishUpload(item, true, body && body.renamed ? 'Saved as ' + body.name : 'Done · ' + fmtBytes(item.file.size));
    } else {
      finishUpload(item, false, (body && body.error) || 'Failed (HTTP ' + xhr.status + ')', true);
    }
  };
  xhr.onerror = () => finishUpload(item, false, 'Interrupted — click Resume', true);
  xhr.onabort = () => finishUpload(item, false, 'Cancelled', false);
  const payload = item.offset > 0 ? item.file.slice(item.offset) : item.file;
  xhr.send(payload);
}

function cancelUpload(item) {
  if (item.xhr && !item.done) item.xhr.abort();
  uploads.queue = uploads.queue.filter((q) => q !== item);
  item.node.remove();
  if (!$('#uploads-list').children.length) $('#uploads').hidden = true;
}

// Recursively traverse dropped FileSystemEntry trees (webkitGetAsEntry).
async function scanDroppedItems(dataTransferItems) {
  const entries = [];
  const fallbackFiles = [];
  for (const item of dataTransferItems) {
    if (item.kind !== 'file') continue;
    const entry = item.webkitGetAsEntry ? item.webkitGetAsEntry() : null;
    if (entry) entries.push({ entry, parent: '' });
    else {
      const f = item.getAsFile();
      if (f) fallbackFiles.push({ file: f, subDir: '' });
    }
  }
  const results = [...fallbackFiles];
  async function walk(entry, parent) {
    if (entry.isFile) {
      const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
      results.push({ file, subDir: parent });
    } else if (entry.isDirectory) {
      const sub = joinPath(parent, entry.name);
      const reader = entry.createReader();
      let batch;
      do {
        batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
        for (const child of batch) await walk(child, sub);
      } while (batch.length > 0);
    }
  }
  for (const { entry, parent } of entries) {
    try { await walk(entry, parent); } catch { /* skip unreadable entry */ }
  }
  return results;
}

let refreshTimer = 0;
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => load(), 400);
}

// ---------- wiring ----------
function init() {
  $('#search-icon').replaceChildren(icon('search'));
  $('#order').replaceChildren(icon('arrow'));
  $('#lb-dl').replaceChildren(icon('download'));
  $('#lb-close').replaceChildren(icon('x'));
  try { const t = localStorage.getItem('share-theme'); if (t) document.documentElement.dataset.theme = t; } catch { /* storage blocked */ }
  applyTheme();
  $('#theme').addEventListener('click', () => {
    const next = applyTheme() ? 'light' : 'dark';
    applyTheme(next);
    try { localStorage.setItem('share-theme', next); } catch { /* storage blocked */ }
  });

  window.addEventListener('click', () => closeAllMenus());

  // Lightbox controls & keyboard navigation.
  $('#lb-close').addEventListener('click', closeLightbox);
  $('#lb-backdrop').addEventListener('click', closeLightbox);
  $('#lb-prev').addEventListener('click', () => stepLightbox(-1));
  $('#lb-next').addEventListener('click', () => stepLightbox(1));
  window.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') closeAllMenus();
    if (!$('#lightbox').hidden) {
      if (ev.key === 'Escape') { ev.preventDefault(); closeLightbox(); }
      else if (ev.key === 'ArrowLeft') { ev.preventDefault(); stepLightbox(-1); }
      else if (ev.key === 'ArrowRight') { ev.preventDefault(); stepLightbox(1); }
    }
  });

  // Auth modal submission.
  $('#auth-form').addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const username = $('#auth-user').value.trim();
    const password = $('#auth-pass').value;
    try {
      await api('/api/auth', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username, password }),
      });
      $('#auth-modal').hidden = true;
      $('#auth-pass').value = '';
      await loadStatus();
      await load();
    } catch {
      const err = $('#auth-err');
      err.textContent = 'Invalid credentials. Please try again.';
      err.hidden = false;
    }
  });

  readLocation();
  $('#q').value = state.q;
  $('#sort').value = state.sort;
  $('#order').dataset.order = state.order;
  $('#rec').checked = state.recursive;

  let debounce = 0;
  $('#q').addEventListener('input', (ev) => {
    clearTimeout(debounce);
    debounce = setTimeout(() => {
      state.q = ev.target.value.trim();
      history.replaceState(null, '', urlFor(state.path));
      load();
    }, 220);
  });
  $('#sort').addEventListener('change', (ev) => { state.sort = ev.target.value; history.replaceState(null, '', urlFor(state.path)); load(); });
  $('#order').addEventListener('click', () => {
    state.order = state.order === 'asc' ? 'desc' : 'asc';
    $('#order').dataset.order = state.order;
    history.replaceState(null, '', urlFor(state.path));
    load();
  });
  $('#rec').addEventListener('change', (ev) => { state.recursive = ev.target.checked; history.replaceState(null, '', urlFor(state.path)); if (state.q) load(); });
  window.addEventListener('popstate', () => {
    readLocation();
    $('#q').value = state.q; $('#sort').value = state.sort; $('#order').dataset.order = state.order; $('#rec').checked = state.recursive;
    load();
  });

  new IntersectionObserver((entries) => {
    if (entries.some((e) => e.isIntersecting)) load({ reset: false });
  }, { rootMargin: '600px' }).observe($('#sentinel'));

  // Uploads: button, drag-and-drop (files and recursive folders), clear.
  $('#upload-btn').addEventListener('click', () => $('#file-input').click());
  $('#file-input').addEventListener('change', (ev) => { enqueue([...ev.target.files]); ev.target.value = ''; });
  $('#uploads-clear').addEventListener('click', () => {
    for (const li of [...$('#uploads-list').children]) if (li.querySelector('button[title="Cancel"][hidden]')) li.remove();
    if (!$('#uploads-list').children.length) $('#uploads').hidden = true;
  });
  let depth = 0;
  const hasFiles = (ev) => ev.dataTransfer && [...ev.dataTransfer.types].includes('Files');
  window.addEventListener('dragenter', (ev) => {
    if (!hasFiles(ev) || !(state.status && state.status.upload_enabled)) return;
    depth++; $('#drop-target').textContent = uploadTarget(); $('#drop').hidden = false;
  });
  window.addEventListener('dragleave', () => { depth = Math.max(0, depth - 1); if (!depth) $('#drop').hidden = true; });
  window.addEventListener('dragover', (ev) => { if (hasFiles(ev)) ev.preventDefault(); });
  window.addEventListener('drop', async (ev) => {
    if (!hasFiles(ev)) return;
    ev.preventDefault(); depth = 0; $('#drop').hidden = true;
    const collected = await scanDroppedItems([...ev.dataTransfer.items]);
    if (collected.length) enqueue(collected);
  });

  // Poll server status periodically so live TUI upload toggles (`[U]`) reflect automatically.
  setInterval(() => {
    if (!document.hidden && $('#auth-modal').hidden) loadStatus();
  }, 2500);

  loadStatus().then(() => load());
}

init();
