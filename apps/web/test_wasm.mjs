// Node test of the wasm interface: encode → decode → bit-exact check.
import fs from 'node:fs';
const [wasmPath, wavPath, args = ''] = process.argv.slice(2);
let mem;
const { instance } = await WebAssembly.instantiate(fs.readFileSync(wasmPath), {
  env: { vfa_progress: () => {} },
});
const X = instance.exports;
console.log('imports ok, exports:', Object.keys(X).filter(k => k.startsWith('vfa_')).length);
const put = (bytes) => { const p = X.vfa_alloc(bytes.length); new Uint8Array(X.memory.buffer, p, bytes.length).set(bytes); return [p, bytes.length]; };
const text = () => new TextDecoder().decode(new Uint8Array(X.memory.buffer, X.vfa_text_ptr(), X.vfa_text_len()));
const result = () => new Uint8Array(X.memory.buffer, X.vfa_result_ptr(), X.vfa_result_len()).slice();
const call = (name, ...bufs) => {
  const ptrs = bufs.map(put);
  const rc = X[name](...ptrs.flat());
  ptrs.forEach(([p, n]) => X.vfa_free(p, n));
  if (rc !== 0) throw new Error(name + ': ' + text());
  return { json: text(), bytes: result() };
};
const enc = new TextEncoder();
const wav = fs.readFileSync(wavPath);
let t = performance.now();
const e = call('vfa_encode', wav, enc.encode(args));
const tEnc = performance.now() - t;
const nx = e.bytes;
t = performance.now();
const d = call('vfa_decode', nx, enc.encode(''));
const tDec = performance.now() - t;
const same = Buffer.compare(Buffer.from(d.bytes), wav) === 0;
const v = call('vfa_verify', wav, nx);
const info = call('vfa_info', nx);
console.log(JSON.stringify({ wav: wav.length, nx: nx.length, ratio: (nx.length / wav.length).toFixed(4), enc_ms: tEnc.toFixed(0), dec_ms: tDec.toFixed(0), wav_byte_identical: same, verify: JSON.parse(v.json).identical }));
console.log(info.json.slice(0, 200));
try { call('vfa_parse', enc.encode('--level 9')); } catch (err) { console.log('parse error ok:', err.message); }
console.log(call('vfa_parse', enc.encode('--level 8 --no-stereo --entropy huffman')).json);
