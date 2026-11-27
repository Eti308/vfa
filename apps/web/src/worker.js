// VFA codec worker: runs the WebAssembly codec off the UI thread.
// Messages in:  {id, op, a?, b?, args?}   (a/b = ArrayBuffer)
// Messages out: {id, type:'progress', stage, done, total}
//               {id, type:'done', ok, text, bytes?, extra?}
let X = null;
let current = 0;
let lastProgress = 0;

function text() {
  return new TextDecoder().decode(new Uint8Array(X.memory.buffer, X.vfa_text_ptr(), X.vfa_text_len()));
}
function result() {
  return new Uint8Array(X.memory.buffer, X.vfa_result_ptr(), X.vfa_result_len()).slice();
}
function put(buf) {
  const bytes = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  const p = X.vfa_alloc(bytes.length);
  new Uint8Array(X.memory.buffer, p, bytes.length).set(bytes);
  return [p, bytes.length];
}
function call(name, ...bufs) {
  const ptrs = bufs.map(put);
  const rc = X[name](...ptrs.flat());
  ptrs.forEach(([p, n]) => X.vfa_free(p, n));
  const t = text();
  const bytes = rc === 0 ? result() : null;
  X.vfa_clear();
  if (rc !== 0) throw new Error(t);
  return { text: t, bytes };
}

// Split interleaved float32 into per-channel arrays for playback; build a
// waveform overview. Non-finite values (NaN/Inf) are silenced in the
// *playback copy only* — the files themselves are never modified.
function preparePlayback(bytes, info) {
  const ch = info.channels, frames = info.frames;
  const inter = new Float32Array(bytes.buffer, bytes.byteOffset, ch * frames);
  const chans = [];
  for (let c = 0; c < ch; c++) chans.push(new Float32Array(frames));
  let peak = 0, nonFinite = 0;
  const BUCKETS = 1600;
  const wmin = new Float32Array(BUCKETS).fill(0), wmax = new Float32Array(BUCKETS).fill(0);
  const per = Math.max(1, frames / BUCKETS);
  for (let f = 0, i = 0; f < frames; f++) {
    const b = Math.min(BUCKETS - 1, Math.floor(f / per));
    for (let c = 0; c < ch; c++, i++) {
      let v = inter[i];
      if (!Number.isFinite(v)) { v = 0; nonFinite++; }
      chans[c][f] = v;
      const a = Math.abs(v);
      if (a > peak) peak = a;
      if (v < wmin[b]) wmin[b] = v;
      if (v > wmax[b]) wmax[b] = v;
    }
  }
  return { chans, peak, nonFinite, wmin, wmax };
}

self.onmessage = async (ev) => {
  const m = ev.data;
  if (m.op === 'init') {
    const { instance } = await WebAssembly.instantiate(m.wasm, {
      env: {
        vfa_progress: (stage, done, total) => {
          const now = performance.now();
          if (now - lastProgress > 60 || done >= total) {
            lastProgress = now;
            self.postMessage({ id: current, type: 'progress', stage, done, total });
          }
        },
      },
    });
    X = instance.exports;
    self.postMessage({ id: m.id, type: 'done', ok: true, text: 'ready' });
    return;
  }
  current = m.id;
  const enc = new TextEncoder();
  try {
    let r, extra = null, transfer = [];
    const t0 = performance.now();
    switch (m.op) {
      case 'encode': r = call('vfa_encode', m.a, enc.encode(m.args || '')); break;
      case 'decode': r = call('vfa_decode', m.a, enc.encode(m.args || '')); break;
      case 'verify': r = call('vfa_verify', m.a, m.b); break;
      case 'info': r = call('vfa_info', m.a); break;
      case 'pcm': {
        r = call('vfa_pcm', m.a);
        const info = JSON.parse(r.text);
        extra = { info, ...preparePlayback(r.bytes, info) };
        transfer = extra.chans.map((c) => c.buffer);
        r.bytes = null;
        break;
      }
      default: throw new Error('unknown op ' + m.op);
    }
    const ms = performance.now() - t0;
    const out = { id: m.id, type: 'done', ok: true, text: r.text, ms, extra };
    if (r.bytes) { out.bytes = r.bytes.buffer; transfer.push(out.bytes); }
    self.postMessage(out, transfer);
  } catch (e) {
    self.postMessage({ id: m.id, type: 'done', ok: false, text: String(e.message || e) });
  }
};
