'use strict';
// VFA desktop app: UI, player, command line. The codec runs in a worker
// (WebAssembly); a second, main-thread instance answers quick queries
// (option parsing, file info, help).

const $ = (id) => document.getElementById(id);
const WASM_BYTES = Uint8Array.from(atob(WASM_B64), (c) => c.charCodeAt(0));

// ------------------------------------------------------------ main-thread codec (quick queries)
let Q = null;
async function initQuick() {
  const { instance } = await WebAssembly.instantiate(WASM_BYTES, { env: { vfa_progress() {} } });
  Q = instance.exports;
}
function quick(name, ...bufs) {
  const enc = new TextEncoder();
  const ptrs = bufs.map((b) => {
    const bytes = typeof b === 'string' ? enc.encode(b) : b;
    const p = Q.vfa_alloc(bytes.length);
    new Uint8Array(Q.memory.buffer, p, bytes.length).set(bytes);
    return [p, bytes.length];
  });
  const rc = Q[name](...ptrs.flat());
  ptrs.forEach(([p, n]) => Q.vfa_free(p, n));
  const text = new TextDecoder().decode(new Uint8Array(Q.memory.buffer, Q.vfa_text_ptr(), Q.vfa_text_len()));
  Q.vfa_clear();
  return { ok: rc === 0, text };
}

// ------------------------------------------------------------ worker jobs
let worker = null, workerReady = null, jobSeq = 0;
const pending = new Map();
let chain = Promise.resolve();

function startWorker() {
  const src = $('worker-src').textContent;
  worker = new Worker(URL.createObjectURL(new Blob([src], { type: 'text/javascript' })));
  worker.onmessage = (ev) => {
    const m = ev.data, p = pending.get(m.id);
    if (!p) return;
    if (m.type === 'progress') { p.onProgress && p.onProgress(m); return; }
    pending.delete(m.id);
    m.ok ? p.resolve(m) : p.reject(new Error(m.text));
  };
  worker.onerror = (e) => {
    for (const p of pending.values()) p.reject(new Error(e.message || 'worker error'));
    pending.clear();
  };
  workerReady = new Promise((resolve, reject) => {
    const id = ++jobSeq;
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, op: 'init', wasm: WASM_BYTES.slice().buffer });
  });
}

/** Queue a job on the worker. `a`/`b` are copied (the originals stay usable). */
function job(op, { a, b, args, onProgress } = {}) {
  const run = async () => {
    await workerReady;
    return new Promise((resolve, reject) => {
      const id = ++jobSeq;
      pending.set(id, { resolve, reject, onProgress });
      const msg = { id, op, args };
      const transfer = [];
      if (a) { msg.a = a.slice().buffer; transfer.push(msg.a); }
      if (b) { msg.b = b.slice().buffer; transfer.push(msg.b); }
      worker.postMessage(msg, transfer);
    });
  };
  const p = chain.then(run, run);
  chain = p.catch(() => {});
  return p;
}

function cancelJobs() {
  worker.terminate();
  for (const p of pending.values()) p.reject(new Error('cancelled'));
  pending.clear();
  chain = Promise.resolve();
  startWorker();
}

// ------------------------------------------------------------ formatting helpers
const fmtBytes = (n) => n >= 1e9 ? (n / 1e9).toFixed(2) + ' GB' : n >= 1e6 ? (n / 1e6).toFixed(2) + ' MB' : n >= 1e3 ? (n / 1e3).toFixed(1) + ' KB' : n + ' B';
function fmtTime(s, tenths = true) {
  if (!Number.isFinite(s) || s < 0) s = 0;
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  const ss = tenths ? sec.toFixed(1).padStart(4, '0') : String(Math.floor(sec)).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}
const fmtRate = (r) => (r % 1000 === 0 ? r / 1000 : (r / 1000).toFixed(1)) + ' kHz';
const baseName = (n) => n.replace(/\.(wav|wave|nx)$/i, '');
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

// ------------------------------------------------------------ console log
const logEl = $('log');
function log(text, cls = '') {
  const line = document.createElement('div');
  if (cls) line.className = cls;
  line.textContent = text;
  logEl.appendChild(line);
  while (logEl.childNodes.length > 400) logEl.removeChild(logEl.firstChild);
  logEl.scrollTop = logEl.scrollHeight;
}

// ------------------------------------------------------------ state
const state = {
  file: null,       // {name, bytes, kind:'wav'|'nx', info}
  wavSource: null,  // last WAV opened (for verify after encode)
  busy: false,
};

// ------------------------------------------------------------ encoder options <-> command line
const MODES_OPTIONAL = ['xorchimp', 'rle', 'fieldvlc', 'xorlz'];
const LEVEL_DESC = [
  'Fastest — fixed blocks, fixed predictors only',
  'Very fast — fixed blocks, short LPC',
  'Fast — fixed blocks, all modes (smaller and faster than format 1.0)',
  'Adaptive block sizes, moderate prediction search',
  'Adaptive block sizes, more prediction orders',
  'Default — adaptive blocks, all modes, balanced speed',
  'Slower — longer LPC search, marginal gains',
  'Slow — 32-tap LPC, wider grid search',
  'Slowest — exhaustive search; rarely smaller than level 5',
];

function controlsToArgs() {
  const L = +$('level').value;
  const parts = [`--level ${L}`];
  const adaptiveDefault = L >= 3;
  const adaptive = $('optAdaptive').checked;
  if (adaptive !== adaptiveDefault) parts.push(adaptive ? '--adaptive' : '--fixed-blocks');
  const blk = $('optBlock').value;
  if (blk !== 'auto') parts.push(`--block ${blk}`);
  const minb = $('optMinBlock').value;
  if (adaptive && minb !== '256') parts.push(`--min-block ${minb}`);
  if (!$('optStereo').checked) parts.push('--no-stereo');
  const modes = ['constant', 'raw', 'floatsplit'];
  let allModes = true;
  for (const m of MODES_OPTIONAL) {
    if (document.querySelector(`[data-mode="${m}"]`).checked) modes.push(m); else allModes = false;
  }
  if ($('optInter').checked) modes.push('floatsplitref', 'xorref'); else allModes = false;
  if (!allModes) parts.push('--modes ' + modes.join(','));
  const coders = [...document.querySelectorAll('[data-coder]')].filter((c) => c.checked).map((c) => c.dataset.coder);
  if (coders.length) parts.push('--entropy rice,' + coders.join(','));
  const idx = $('optIndex').value;
  if (idx !== 'compact') parts.push(`--index ${idx}`);
  if (!$('optHeader').checked) parts.push('--no-wav-header');
  return parts.join(' ');
}

function decodeArgs() {
  const parts = [];
  const st = $('exStart').value.trim(), du = $('exDur').value.trim();
  if (st !== '' && +st > 0) parts.push(`--start-time ${+st}`);
  if (du !== '') parts.push(`--duration ${+du}`);
  return parts.join(' ');
}

function setSelect(sel, value) {
  const v = String(value);
  if (![...sel.options].some((o) => o.value === v)) {
    const o = document.createElement('option');
    o.value = o.textContent = v;
    sel.appendChild(o);
  }
  sel.value = v;
}

/** Apply a parsed configuration (from the command line) to the controls. */
function configToControls(cfg, decode) {
  $('level').value = cfg.level;
  $('optStereo').checked = cfg.stereo;
  $('optAdaptive').checked = cfg.adaptive;
  const defaultBlock = cfg.adaptive ? 8192 : 2048;
  setSelect($('optBlock'), cfg.block === defaultBlock ? 'auto' : cfg.block);
  setSelect($('optMinBlock'), cfg.min_block);
  $('optIndex').value = cfg.index;
  for (const c of document.querySelectorAll('[data-coder]')) c.checked = cfg.entropy.includes(c.dataset.coder);
  for (const m of MODES_OPTIONAL) document.querySelector(`[data-mode="${m}"]`).checked = cfg.modes.includes(m);
  $('optInter').checked = cfg.modes.includes('floatsplitref') || cfg.modes.includes('xorref');
  $('optHeader').checked = cfg.wav_header;
  if (decode) {
    $('exStart').value = decode['--start-time'] ?? '';
    $('exDur').value = decode['--duration'] ?? '';
  }
  refreshOptionViews();
}

function refreshOptionViews() {
  const L = +$('level').value;
  $('levelValue').textContent = L;
  $('levelDesc').textContent = LEVEL_DESC[L];
  setRangeFill($('level'));
  $('encPreview').textContent = 'vfa encode ' + (state.file ? quoteName(state.file.name) + ' ' + quoteName(baseName(state.file.name) + '.nx') + ' ' : '') + controlsToArgs();
  const da = decodeArgs();
  $('decPreview').textContent = 'vfa decode ' + (state.file ? quoteName(state.file.name) + ' ' + quoteName(baseName(state.file.name) + '.wav') + ' ' : '') + da;
  if (document.activeElement !== $('cmd')) {
    $('cmd').value = state.file && state.file.kind === 'nx' ? da : controlsToArgs();
    validateCmd();
  }
}
const quoteName = (n) => (/\s/.test(n) ? `"${n}"` : n);

// ------------------------------------------------------------ file handling
async function openFile(file) {
  const bytes = new Uint8Array(await file.arrayBuffer());
  const kind = bytes.length >= 4 && bytes[0] === 0x56 && bytes[1] === 0x46 && bytes[2] === 0x41 && bytes[3] === 0x1a ? 'nx' : 'wav';
  const r = quick('vfa_info', bytes);
  if (!r.ok) {
    log(`✗ ${file.name}: ${r.text}`, 'err');
    showResult(`<div class="result-line"><span class="badge err">Can't open</span><strong>${esc(file.name)}</strong></div><p class="error-text">${esc(r.text)}</p>`);
    return;
  }
  const info = JSON.parse(r.text);
  loadIntoUi({ name: file.name, bytes, kind, info });
}

function loadIntoUi(f) {
  player.unload();
  state.file = f;
  if (f.kind === 'wav') state.wavSource = f;
  $('drop').classList.add('hidden');
  $('fileCard').classList.remove('hidden');
  $('fileName').textContent = f.name;
  const icon = $('fileIcon');
  icon.textContent = f.kind === 'nx' ? 'NX' : 'WAV';
  icon.classList.toggle('nx', f.kind === 'nx');
  const i = f.info;
  const dur = i.frames / i.sample_rate;
  const chText = i.channels === 1 ? 'mono' : i.channels === 2 ? 'stereo' : `${i.channels} ch`;
  let details;
  if (f.kind === 'nx') {
    details = `VFA ${i.version} · ${chText} · ${fmtRate(i.sample_rate)} · ${fmtTime(dur)} · ${fmtBytes(i.bytes)} — ${(100 * i.bytes / i.wav_bytes).toFixed(1)} % of the ${fmtBytes(i.wav_bytes)} WAV · ${i.bits_per_sample.toFixed(2)} bits/sample · ${i.blocks} blocks`;
    $('decHint').textContent = i.wav_header ? 'original WAV header stored → byte-identical file' : 'no original header stored → standard float32 WAV';
  } else {
    const fmt = i.format === 'float32' ? '32-bit float' : `${i.format.slice(3)}-bit integer`;
    details = `WAV ${fmt} · ${chText} · ${fmtRate(i.sample_rate)} · ${fmtTime(dur)} · ${fmtBytes(i.bytes)}`;
    $('encHint').textContent = i.format === 'float32' ? '' : `${fmt} source is converted exactly to float32 (decoded WAV will be float32)`;
  }
  $('fileDetails').textContent = details;
  $('encoderPanel').classList.toggle('hidden', f.kind !== 'wav');
  $('decoderPanel').classList.toggle('hidden', f.kind !== 'nx');
  log(`opened ${f.name} — ${details}`, 'dim');
  if (f.kind === 'nx' && i.encoder) log(`  encoder: ${i.encoder}`, 'dim');
  refreshOptionViews();
  player.load(f);
}

function closeFile() {
  player.unload();
  state.file = null;
  $('fileCard').classList.add('hidden');
  $('encoderPanel').classList.add('hidden');
  $('decoderPanel').classList.add('hidden');
  $('result').classList.add('hidden');
  $('drop').classList.remove('hidden');
  refreshOptionViews();
}

/** Save output: a download by default; with `picker` (from a button click) a Save dialog where supported. */
async function saveBytes(bytes, name, mime, picker = false) {
  if (picker && window.showSaveFilePicker) {
    try {
      const ext = name.slice(name.lastIndexOf('.'));
      const h = await window.showSaveFilePicker({ suggestedName: name, types: [{ description: ext.toUpperCase().slice(1) + ' file', accept: { [mime]: [ext] } }] });
      const w = await h.createWritable();
      await w.write(bytes);
      await w.close();
      return `saved as ${h.name}`;
    } catch (e) {
      if (e && e.name === 'AbortError') return 'save cancelled';
      // not allowed without a user gesture etc. — fall back to a download
    }
  }
  const url = URL.createObjectURL(new Blob([bytes], { type: mime }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
  return `downloaded ${name}`;
}

// ------------------------------------------------------------ progress UI
const STAGES = { 1: 'Encoding', 2: 'Decoding', 3: 'Verifying' };
function progressStart(title) {
  state.busy = true;
  $('progress').classList.remove('hidden');
  $('progTitle').textContent = title;
  $('progBar').style.width = '0%';
  $('progText').textContent = '';
  $('encodeBtn').disabled = $('decodeBtn').disabled = true;
  return performance.now();
}
function progressUpdate(m, t0, rate) {
  const pct = m.total ? (100 * m.done) / m.total : 0;
  $('progBar').style.width = pct.toFixed(1) + '%';
  const secs = (performance.now() - t0) / 1000;
  const speed = rate && secs > 0.05 ? ` · ${((m.done / rate) / secs).toFixed(0)}× real time` : '';
  $('progTitle').textContent = `${STAGES[m.stage] || 'Working'}… ${pct.toFixed(0)} %`;
  $('progText').textContent = `${fmtTime(m.done / (rate || 1))} of ${fmtTime(m.total / (rate || 1))} audio · ${secs.toFixed(1)} s elapsed${speed}`;
}
function progressEnd() {
  state.busy = false;
  $('progress').classList.add('hidden');
  $('encodeBtn').disabled = $('decodeBtn').disabled = false;
}
function showResult(html) {
  const r = $('result');
  r.innerHTML = html;
  r.classList.remove('hidden');
}

// ------------------------------------------------------------ actions
async function doEncode(argsOverride) {
  const f = state.file;
  if (!f || f.kind !== 'wav') { log('encode: open a .wav file first', 'err'); return; }
  if (state.busy) { log('busy — wait or cancel', 'err'); return; }
  const args = argsOverride ?? controlsToArgs();
  const outName = baseName(f.name) + '.nx';
  log(`vfa encode ${quoteName(f.name)} ${quoteName(outName)} ${args}`, 'in');
  const t0 = progressStart('Encoding…');
  const rate = f.info.sample_rate;
  try {
    const r = await job('encode', { a: f.bytes, args, onProgress: (m) => progressUpdate(m, t0, rate) });
    const tEnc = (performance.now() - t0) / 1000;
    const s = JSON.parse(r.text);
    const nx = new Uint8Array(r.bytes);
    let verify = null;
    if ($('optVerify').checked) {
      const t1 = performance.now();
      const v = await job('verify', { a: f.bytes, b: nx, onProgress: (m) => progressUpdate(m, t1, rate) });
      verify = JSON.parse(v.text);
    }
    progressEnd();
    const saved = await saveBytes(nx, outName, 'application/octet-stream');
    const pct = (100 * s.output_bytes) / s.input_bytes;
    const dur = s.frames / s.sample_rate;
    const modes = s.summary.modes.filter((m) => m.share >= 0.005).sort((a, b) => b.share - a.share).map((m) => `${m.mode} ${(100 * m.share).toFixed(0)} %`).join(', ');
    const vBadge = verify == null ? '' : verify.identical
      ? `<span class="badge ok">✓ bit-exact — ${verify.samples.toLocaleString()} samples verified${verify.wav_file_identical ? ', WAV byte-identical' : ''}</span>`
      : `<span class="badge err">✗ verification failed (${verify.mismatches} mismatches)</span>`;
    showResult(`
      <div class="result-line"><strong>${esc(outName)}</strong><span class="muted">${esc(saved)}</span>${vBadge}</div>
      <div class="stats">
        <div class="stat"><b>${pct.toFixed(1)} %</b><span>of original size</span></div>
        <div class="stat"><b>${fmtBytes(s.output_bytes)}</b><span>from ${fmtBytes(s.input_bytes)}</span></div>
        <div class="stat"><b>${s.bits_per_sample.toFixed(2)}</b><span>bits per sample (32 raw)</span></div>
        <div class="stat"><b>${(dur / tEnc).toFixed(1)}×</b><span>real time · ${tEnc.toFixed(1)} s</span></div>
      </div>
      <div class="muted">${s.summary.blocks} blocks · modes: ${esc(modes)}${s.source !== 'float32' ? ' · source ' + esc(s.source) + ' converted exactly to float32' : ''}</div>
      <div class="result-buttons"><button class="btn" id="saveAgain">Save As…</button><button class="btn" id="openOut">Open .nx in Player</button></div>`);
    $('saveAgain').onclick = async () => log(await saveBytes(nx, outName, 'application/octet-stream', true), 'dim');
    $('openOut').onclick = () => {
      const r2 = quick('vfa_info', nx);
      loadIntoUi({ name: outName, bytes: nx, kind: 'nx', info: JSON.parse(r2.text) });
    };
    log(`✓ ${outName}: ${fmtBytes(s.output_bytes)} (${pct.toFixed(2)} %), ${s.bits_per_sample.toFixed(2)} bits/sample, ${tEnc.toFixed(2)} s${verify ? (verify.identical ? ' — bit-exact verified' : ' — VERIFY FAILED') : ''} — ${saved}`, verify && !verify.identical ? 'err' : 'ok');
  } catch (e) {
    progressEnd();
    log(`✗ encode: ${e.message}`, 'err');
    if (e.message !== 'cancelled') showResult(`<div class="result-line"><span class="badge err">Encoding failed</span></div><p class="error-text">${esc(e.message)}</p>`);
  }
}

async function doDecode(argsOverride) {
  const f = state.file;
  if (!f || f.kind !== 'nx') { log('decode: open a .nx file first', 'err'); return; }
  if (state.busy) { log('busy — wait or cancel', 'err'); return; }
  const args = argsOverride ?? decodeArgs();
  const outName = baseName(f.name) + (args ? '-excerpt' : '') + '.wav';
  log(`vfa decode ${quoteName(f.name)} ${quoteName(outName)} ${args}`, 'in');
  const t0 = progressStart('Decoding…');
  try {
    const r = await job('decode', { a: f.bytes, args, onProgress: (m) => progressUpdate(m, t0, f.info.sample_rate) });
    const secs = (performance.now() - t0) / 1000;
    const s = JSON.parse(r.text);
    const wav = new Uint8Array(r.bytes);
    progressEnd();
    const saved = await saveBytes(wav, outName, 'audio/wav');
    const badge = s.original_header ? '<span class="badge ok">✓ byte-identical to the original WAV</span>' : s.excerpt ? '<span class="badge ok">excerpt · float32 WAV</span>' : '<span class="badge ok">float32 WAV</span>';
    showResult(`
      <div class="result-line"><strong>${esc(outName)}</strong><span class="muted">${esc(saved)}</span>${badge}</div>
      <div class="stats">
        <div class="stat"><b>${fmtBytes(s.output_bytes)}</b><span>WAV size</span></div>
        <div class="stat"><b>${secs.toFixed(2)} s</b><span>decode time</span></div>
      </div>
      <div class="result-buttons"><button class="btn" id="saveAgain">Save As…</button></div>`);
    $('saveAgain').onclick = async () => log(await saveBytes(wav, outName, 'audio/wav', true), 'dim');
    log(`✓ ${outName}: ${fmtBytes(s.output_bytes)} in ${secs.toFixed(2)} s${s.original_header ? ' — original header restored (byte-identical)' : ''} — ${saved}`, 'ok');
  } catch (e) {
    progressEnd();
    log(`✗ decode: ${e.message}`, 'err');
    if (e.message !== 'cancelled') showResult(`<div class="result-line"><span class="badge err">Decoding failed</span></div><p class="error-text">${esc(e.message)}</p>`);
  }
}

// ------------------------------------------------------------ player (QuickTime style)
const player = (() => {
  let ctx = null, gain = null, buffer = null, source = null;
  let pcm = null;               // {chans, rate, frames, peak, nonFinite, wmin, wmax}
  let duration = 0, offset = 0, startedAt = 0, playing = false, loop = false, raf = 0, loadToken = 0;
  const canvas = $('wave'), seek = $('seek');

  function position() {
    if (!playing) return offset;
    return Math.min(duration, offset + (ctx.currentTime - startedAt));
  }
  function setPlayIcon() {
    $('playIcon').innerHTML = playing ? '<path d="M6.5 4.5h4v15h-4zM13.5 4.5h4v15h-4z"/>' : '<path d="M7 4.5v15l13-7.5z"/>';
    $('playBtn').title = playing ? 'Pause (space)' : 'Play (space)';
  }
  function enable(on) {
    for (const id of ['playBtn', 'back10', 'fwd10', 'seek']) $(id).disabled = !on;
  }
  function applyGain() {
    if (!gain) return;
    const norm = $('normalize').checked && pcm && pcm.peak > 0 ? 0.98 / pcm.peak : 1;
    gain.gain.value = +$('volume').value * norm;
  }
  function draw() {
    const dpr = window.devicePixelRatio || 1;
    const w = Math.max(1, Math.round(canvas.clientWidth * dpr)), h = Math.max(1, Math.round(canvas.clientHeight * dpr));
    if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
    const g = canvas.getContext('2d');
    g.clearRect(0, 0, w, h);
    const mid = h / 2;
    if (!pcm) {
      g.fillStyle = 'rgba(255,255,255,0.18)';
      g.fillRect(0, mid - 0.5 * dpr, w, dpr);
      return;
    }
    const n = pcm.wmin.length;
    let peak = 0;
    for (let i = 0; i < n; i++) peak = Math.max(peak, -pcm.wmin[i], pcm.wmax[i]);
    const scale = peak > 0 ? (mid - 2 * dpr) / peak : 0;
    const played = duration ? position() / duration : 0;
    const bar = Math.max(1, Math.round(2 * dpr)), gap = Math.max(1, Math.round(dpr));
    for (let x = 0; x < w; x += bar + gap) {
      const i0 = Math.floor((x / w) * n), i1 = Math.max(i0 + 1, Math.floor(((x + bar) / w) * n));
      let lo = 0, hi = 0;
      for (let i = i0; i < i1 && i < n; i++) { lo = Math.min(lo, pcm.wmin[i]); hi = Math.max(hi, pcm.wmax[i]); }
      const top = mid - hi * scale, bottom = mid - lo * scale;
      g.fillStyle = x / w < played ? '#0a84ff' : 'rgba(255,255,255,0.5)';
      g.fillRect(x, top, bar, Math.max(dpr, bottom - top));
    }
    const px = Math.round(played * w);
    g.fillStyle = '#fff';
    g.fillRect(px - dpr, 0, 2 * dpr, h);
  }
  function updateUi() {
    const p = position();
    $('tElapsed').textContent = fmtTime(p);
    $('tRemain').textContent = '-' + fmtTime(duration - p);
    if (!seek.dragging) { seek.value = duration ? Math.round((p / duration) * 1000) : 0; setRangeFill(seek); }
    draw();
  }
  function tick() {
    updateUi();
    if (playing) raf = requestAnimationFrame(tick);
  }
  function ensureContext() {
    if (!ctx) {
      const AC = window.AudioContext || window.webkitAudioContext;
      ctx = new AC();
      gain = ctx.createGain();
      gain.connect(ctx.destination);
    }
    if (!buffer && pcm) {
      buffer = ctx.createBuffer(pcm.chans.length, pcm.frames, pcm.rate);
      pcm.chans.forEach((c, i) => buffer.copyToChannel(c, i));
      pcm.chans = null; // the AudioBuffer now holds the samples
    }
    applyGain();
  }
  function stopSource() {
    if (source) { source.onended = null; try { source.stop(); } catch (_) {} source.disconnect(); source = null; }
  }
  function play() {
    if (!pcm || playing) return;
    try { ensureContext(); } catch (e) { log(`✗ playback: ${e.message}`, 'err'); return; }
    if (ctx.state === 'suspended') ctx.resume();
    if (offset >= duration - 0.01) offset = 0;
    source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(gain);
    source.onended = () => {
      playing = false; source = null;
      if (loop) { offset = 0; play(); } else { offset = duration; setPlayIcon(); updateUi(); }
    };
    source.start(0, offset);
    startedAt = ctx.currentTime;
    playing = true;
    setPlayIcon();
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(tick);
  }
  function pause() {
    if (!playing) return;
    offset = position();
    playing = false;
    stopSource();
    setPlayIcon();
    updateUi();
  }
  function seekTo(t) {
    const was = playing;
    if (playing) { offset = position(); playing = false; stopSource(); }
    offset = Math.max(0, Math.min(duration, t));
    if (was) play(); else updateUi();
  }
  async function load(f) {
    const token = ++loadToken;
    pcm = null; buffer = null; duration = f.info.frames / f.info.sample_rate; offset = 0;
    enable(false);
    $('peakInfo').textContent = '';
    $('playerLoading').classList.remove('hidden');
    $('playerLoading').textContent = f.kind === 'nx' ? 'Decoding for playback…' : 'Preparing audio…';
    updateUi();
    try {
      const r = await job('pcm', { a: f.bytes, onProgress: (m) => { if (token === loadToken) $('playerLoading').textContent = `Decoding for playback… ${Math.round((100 * m.done) / m.total)} %`; } });
      if (token !== loadToken) return;
      const x = r.extra;
      pcm = { chans: x.chans, rate: x.info.sample_rate, frames: x.info.frames, peak: x.peak, nonFinite: x.nonFinite, wmin: x.wmin, wmax: x.wmax };
      const db = pcm.peak > 0 ? 20 * Math.log10(pcm.peak) : -Infinity;
      $('normalize').checked = pcm.peak > 1;
      let note = pcm.peak > 0 ? `peak ${db >= 0 ? '+' : ''}${db.toFixed(1)} dBFS` : 'digital silence';
      if (pcm.peak > 1) note += ' — above full scale, normalized so it does not clip';
      if (pcm.nonFinite) note += ` · ${pcm.nonFinite.toLocaleString()} NaN/Inf samples muted for playback`;
      $('peakInfo').textContent = note;
      $('playerLoading').classList.add('hidden');
      enable(true);
      updateUi();
    } catch (e) {
      if (token !== loadToken) return;
      $('playerLoading').textContent = 'Playback unavailable: ' + e.message;
    }
  }
  function unload() {
    loadToken++;
    pause();
    stopSource();
    pcm = null; buffer = null; duration = 0; offset = 0;
    enable(false);
    updateUi();
  }
  // controls
  $('playBtn').onclick = () => (playing ? pause() : play());
  $('back10').onclick = () => seekTo(position() - 10);
  $('fwd10').onclick = () => seekTo(position() + 10);
  $('loopBtn').onclick = () => { loop = !loop; $('loopBtn').classList.toggle('on', loop); };
  $('volume').oninput = () => { setRangeFill($('volume')); applyGain(); };
  $('normalize').onchange = applyGain;
  seek.addEventListener('pointerdown', () => { seek.dragging = true; });
  seek.addEventListener('input', () => {
    setRangeFill(seek);
    const t = (seek.value / 1000) * duration;
    $('tElapsed').textContent = fmtTime(t);
    $('tRemain').textContent = '-' + fmtTime(duration - t);
  });
  seek.addEventListener('change', () => { seek.dragging = false; seekTo((seek.value / 1000) * duration); });
  let scrubbing = false;
  const fromEvent = (e) => { const r = canvas.getBoundingClientRect(); return ((e.clientX - r.left) / r.width) * duration; };
  canvas.addEventListener('pointerdown', (e) => { if (!pcm) return; scrubbing = true; canvas.setPointerCapture(e.pointerId); seekTo(fromEvent(e)); });
  canvas.addEventListener('pointermove', (e) => { if (scrubbing) seekTo(fromEvent(e)); });
  canvas.addEventListener('pointerup', () => { scrubbing = false; });
  window.addEventListener('resize', draw);
  return { load, unload, play, pause, seekTo, position, get playing() { return playing; }, get ready() { return !!pcm; }, get duration() { return duration; } };
})();

function setRangeFill(r) {
  const pct = ((r.value - r.min) / (r.max - r.min || 1)) * 100;
  r.style.setProperty('--pct', pct + '%');
}

// ------------------------------------------------------------ command line
const cmd = $('cmd'), cmdStatus = $('cmdStatus');
const cmdHistory = [];
let histPos = 0;
const VERBS = ['encode', 'decode', 'play', 'pause', 'stop', 'info', 'verify', 'help', 'clear', 'reset', 'open', 'close'];

function splitCommand(line) {
  let toks = line.trim().split(/\s+/).filter(Boolean);
  if (toks[0] === 'vfa') toks = toks.slice(1);
  const verb = toks.length && VERBS.includes(toks[0].toLowerCase()) ? toks.shift().toLowerCase() : null;
  // keep only options (file names come from the window)
  const opts = [];
  for (let i = 0; i < toks.length; i++) {
    if (toks[i].startsWith('--')) {
      opts.push(toks[i]);
      if (!toks[i].includes('=') && i + 1 < toks.length && !toks[i + 1].startsWith('--') && VALUE_FLAGS.includes(toks[i])) opts.push(toks[++i]);
    }
  }
  return { verb, opts: opts.join(' ') };
}
const VALUE_FLAGS = ['--level', '--block', '--modes', '--threads', '--start', '--frames', '--start-time', '--duration', '--min-block', '--index', '--entropy'];

function describe(cfg) {
  const parts = [`level ${cfg.level}`];
  parts.push(cfg.adaptive ? `adaptive blocks ${cfg.min_block}–${cfg.block}` : `fixed blocks ${cfg.block}`);
  parts.push(cfg.stereo ? 'stereo on' : 'stereo off');
  const inter = cfg.modes.includes('floatsplitref') || cfg.modes.includes('xorref');
  parts.push(inter ? 'inter-channel on' : 'inter-channel off');
  if (cfg.entropy.length > 1) parts.push('coders ' + cfg.entropy.join('+'));
  if (cfg.index !== 'compact') parts.push(`index ${cfg.index}`);
  if (!cfg.wav_header) parts.push('no WAV header');
  return parts.join(' · ');
}

function validateCmd() {
  const { verb, opts } = splitCommand(cmd.value);
  const nxOpen = state.file && state.file.kind === 'nx';
  // an empty line describes what the window is set to right now
  const r = quick('vfa_parse', opts || (nxOpen ? decodeArgs() : controlsToArgs()));
  if (!r.ok) {
    cmdStatus.className = 'console-status err';
    cmdStatus.textContent = '✗ ' + r.text.replace(/^invalid input: /, '');
    return null;
  }
  const p = JSON.parse(r.text);
  cmdStatus.className = 'console-status ok';
  const dec = Object.keys(p.decode).length ? ' · excerpt ' + Object.entries(p.decode).map(([k, v]) => `${k.slice(2)} ${v}`).join(', ') : '';
  const what = nxOpen && verb !== 'encode'
    ? 'decode ' + (dec ? dec.slice(3) : 'whole file' + (state.file.info.wav_header ? ' → byte-identical WAV' : ' → float32 WAV'))
    : describe(p.config) + dec;
  cmdStatus.textContent = (verb ? `${verb}: ` : '✓ ') + what;
  return p;
}

const HELP_APP = `app commands
  encode [options]            encode the open WAV (options also update the controls)
  decode [options]            decode the open .nx to WAV
  play · pause · stop         playback control (space bar also toggles)
  info                        details of the open file
  verify                      re-encode-free check: decode the open .nx against the last WAV
  open · close · clear · reset · help
typing options alone (e.g. --level 8 --no-stereo) sets them for the next encode`;

async function runCommand(line) {
  const { verb, opts } = splitCommand(line);
  const parsed = validateCmd();
  if (line.trim()) log('vfa ❯ ' + line.trim(), 'in');
  if (!parsed && verb !== 'help' && verb !== 'clear') { log('✗ ' + cmdStatus.textContent.slice(2), 'err'); return; }
  if (parsed && opts) configToControls(parsed.config, parsed.decode);
  switch (verb) {
    case null:
      if (opts) log('options set: ' + describe(parsed.config), 'dim');
      break;
    case 'encode': await doEncode(opts ? controlsToArgs() : undefined); break;
    case 'decode': await doDecode(opts ? decodeArgs() : undefined); break;
    case 'play': player.play(); break;
    case 'pause': case 'stop': player.pause(); if (verb === 'stop') player.seekTo(0); break;
    case 'info':
      if (!state.file) log('no file open', 'err');
      else log(JSON.stringify(state.file.info, null, 1).replace(/[{}"]/g, '').replace(/,\n/g, '\n').trim(), 'dim');
      break;
    case 'verify': {
      const f = state.file, w = state.wavSource;
      if (!f || f.kind !== 'nx' || !w) { log('verify: open the original WAV, encode it, then open the .nx (or use "Verify after encoding")', 'err'); break; }
      const t0 = progressStart('Verifying…');
      try {
        const v = JSON.parse((await job('verify', { a: w.bytes, b: f.bytes, onProgress: (m) => progressUpdate(m, t0, f.info.sample_rate) })).text);
        log(v.identical ? `✓ ${f.name} matches ${w.name} bit for bit (${v.samples.toLocaleString()} samples)` : `✗ ${v.mismatches} mismatching samples`, v.identical ? 'ok' : 'err');
      } catch (e) { log('✗ verify: ' + e.message, 'err'); }
      progressEnd();
      break;
    }
    case 'help':
      log(quick('vfa_help').text.trimEnd(), 'dim');
      log(HELP_APP, 'dim');
      break;
    case 'clear': logEl.textContent = ''; break;
    case 'reset':
      configToControls(JSON.parse(quick('vfa_parse', '').text).config, { '--start-time': '', '--duration': '' });
      log('options reset to defaults', 'dim');
      break;
    case 'open': $('fileInput').click(); break;
    case 'close': closeFile(); break;
  }
  refreshOptionViews();
}

cmd.addEventListener('input', validateCmd);
cmd.addEventListener('keydown', async (e) => {
  if (e.key === 'Enter') {
    const line = cmd.value;
    if (line.trim()) { cmdHistory.push(line); histPos = cmdHistory.length; }
    cmd.blur();
    await runCommand(line);
    cmd.focus();
    cmd.value = state.file && state.file.kind === 'nx' ? decodeArgs() : controlsToArgs();
    validateCmd();
  } else if (e.key === 'ArrowUp' && cmdHistory.length) {
    histPos = Math.max(0, histPos - 1); cmd.value = cmdHistory[histPos]; validateCmd(); e.preventDefault();
  } else if (e.key === 'ArrowDown' && cmdHistory.length) {
    histPos = Math.min(cmdHistory.length, histPos + 1); cmd.value = cmdHistory[histPos] ?? ''; validateCmd(); e.preventDefault();
  }
});

// ------------------------------------------------------------ wiring
function wire() {
  $('openBtn').onclick = $('chooseBtn').onclick = () => $('fileInput').click();
  $('fileInput').onchange = (e) => { const f = e.target.files[0]; if (f) openFile(f); e.target.value = ''; };
  $('closeFile').onclick = closeFile;
  $('encodeBtn').onclick = () => doEncode();
  $('decodeBtn').onclick = () => doDecode();
  $('cancelBtn').onclick = () => { cancelJobs(); progressEnd(); log('cancelled', 'err'); if (state.file) player.load(state.file); };
  for (const el of document.querySelectorAll('#encoderPanel input, #encoderPanel select, #decoderPanel input')) {
    el.addEventListener('input', refreshOptionViews);
    el.addEventListener('change', refreshOptionViews);
  }
  $('level').addEventListener('input', () => {
    // level presets decide the default block mode; follow them unless the user overrides later
    $('optAdaptive').checked = +$('level').value >= 3;
    refreshOptionViews();
  });
  // drag & drop anywhere in the window
  let depth = 0;
  window.addEventListener('dragenter', (e) => { e.preventDefault(); depth++; document.body.classList.add('dragging'); });
  window.addEventListener('dragleave', () => { if (--depth <= 0) { depth = 0; document.body.classList.remove('dragging'); } });
  window.addEventListener('dragover', (e) => e.preventDefault());
  window.addEventListener('drop', (e) => {
    e.preventDefault(); depth = 0; document.body.classList.remove('dragging');
    const f = e.dataTransfer.files[0];
    if (f) openFile(f);
  });
  // keyboard shortcuts (not while typing)
  window.addEventListener('keydown', (e) => {
    const tag = (document.activeElement && document.activeElement.tagName) || '';
    if (['INPUT', 'SELECT', 'TEXTAREA'].includes(tag) && document.activeElement.type !== 'range' && document.activeElement.type !== 'checkbox') return;
    if (e.metaKey && e.key.toLowerCase() === 'o') { e.preventDefault(); $('fileInput').click(); return; }
    if (!player.ready) return;
    if (e.code === 'Space') { e.preventDefault(); player.playing ? player.pause() : player.play(); }
    else if (e.key === 'ArrowLeft') { e.preventDefault(); player.seekTo(player.position() - 5); }
    else if (e.key === 'ArrowRight') { e.preventDefault(); player.seekTo(player.position() + 5); }
    else if (e.key === 'Home') { e.preventDefault(); player.seekTo(0); }
  });
  setRangeFill($('volume'));
}

(async function main() {
  try {
    await initQuick();
    startWorker();
    await workerReady;
  } catch (e) {
    document.body.insertAdjacentHTML('afterbegin', `<p style="padding:20px;color:#d70015">Could not start the codec: ${esc(e.message)}</p>`);
    return;
  }
  wire();
  player.unload(); // controls disabled until a file is loaded
  refreshOptionViews();
  log('VFA Encoder ready — drop a .wav or .nx file, or type "help". Everything runs locally.', 'dim');
})();
