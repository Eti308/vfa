// End-to-end UI test of dist/VFA.html in headless Chromium (Playwright).
// Usage: node test_ui.js <float32.wav> <vfa-cli-binary> <out-dir>
const { chromium } = require('playwright');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const [wavPath, cli, outDir] = process.argv.slice(2);
const page_url = 'file://' + path.join(__dirname, 'dist', 'VFA.html');
const results = [];
const check = (name, ok, detail = '') => { results.push({ name, ok }); console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`); };

function writeInt24Wav(file, rate, ch, frames) {
  const data = Buffer.alloc(frames * ch * 3);
  for (let i = 0; i < frames * ch; i++) {
    const v = Math.round(Math.sin(i * 0.013) * 4_000_000 + (i % 7) * 1000);
    data.writeIntLE(v, i * 3, 3);
  }
  const h = Buffer.alloc(44);
  h.write('RIFF', 0); h.writeUInt32LE(36 + data.length, 4); h.write('WAVEfmt ', 8);
  h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(ch, 22); h.writeUInt32LE(rate, 24);
  h.writeUInt32LE(rate * ch * 3, 28); h.writeUInt16LE(ch * 3, 32); h.writeUInt16LE(24, 34);
  h.write('data', 36); h.writeUInt32LE(data.length, 40);
  fs.writeFileSync(file, Buffer.concat([h, data]));
}

(async () => {
  fs.mkdirSync(outDir, { recursive: true });
  const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
  const context = await browser.newContext({ acceptDownloads: true, viewport: { width: 980, height: 1100 }, deviceScaleFactor: 2 });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  const nextDownload = async (trigger) => {
    const [d] = await Promise.all([page.waitForEvent('download', { timeout: 120000 }), trigger()]);
    const p = path.join(outDir, d.suggestedFilename());
    await d.saveAs(p);
    return p;
  };
  const command = async (line) => { await page.fill('#cmd', line); await page.press('#cmd', 'Enter'); };

  await page.goto(page_url);
  await page.waitForFunction(() => document.getElementById('log').textContent.includes('ready'), null, { timeout: 30000 });
  check('app starts (wasm + worker)', true);
  await page.screenshot({ path: path.join(outDir, 'shot_empty.png') });

  // open a float32 WAV
  check('player disabled before a file is open', await page.isDisabled('#playBtn'));
  await page.setInputFiles('#fileInput', wavPath);
  await page.waitForFunction(() => document.getElementById('fileName').textContent && !document.getElementById('playBtn').disabled, null, { timeout: 60000 });
  check('WAV opens, player ready', await page.isVisible('#encoderPanel'), await page.textContent('#fileDetails'));

  // command line drives the controls
  await command('--level 2 --no-stereo');
  const lvl = await page.inputValue('#level');
  const stereo = await page.isChecked('#optStereo');
  check('command line sets controls', lvl === '2' && !stereo, `level=${lvl} stereo=${stereo}`);
  await page.fill('#cmd', '--bogus');
  const status = await page.textContent('#cmdStatus');
  check('invalid option reported live', status.includes('unknown option --bogus'), status.trim());
  // like the CLI, an option line describes the complete configuration
  await command('--level 5');
  check('option line = complete configuration (CLI semantics)', await page.isChecked('#optStereo'));
  // controls drive the command line
  await page.click('#optStereo');
  const preview = await page.textContent('#encPreview');
  check('controls update command preview', preview.includes('--level 5') && preview.includes('--no-stereo'), preview);
  check('command field shows the effective options', (await page.inputValue('#cmd')).includes('--no-stereo'));

  // encode via button
  const nxPath = await nextDownload(() => page.click('#encodeBtn'));
  await page.waitForSelector('#result .badge', { timeout: 120000 });
  const resultText = await page.textContent('#result');
  check('encode produces .nx and verifies bit-exact', resultText.includes('bit-exact') && fs.statSync(nxPath).size > 0, `${fs.statSync(nxPath).size} bytes`);
  await page.screenshot({ path: path.join(outDir, 'shot_encoded.png') });

  // the downloaded .nx decodes with the native CLI to the identical WAV
  const back = path.join(outDir, 'native_back.wav');
  execFileSync(cli, ['decode', nxPath, back]);
  check('native CLI decodes app output byte-identically', Buffer.compare(fs.readFileSync(back), fs.readFileSync(wavPath)) === 0);

  // open the .nx in the player and play
  await page.click('#openOut');
  await page.waitForFunction(() => document.getElementById('fileName').textContent.endsWith('.nx') && !document.getElementById('playBtn').disabled, null, { timeout: 60000 });
  check('.nx opens in player', await page.isVisible('#decoderPanel'), await page.textContent('#fileDetails'));
  await page.click('#playBtn');
  await page.waitForTimeout(1500);
  const t1 = await page.textContent('#tElapsed');
  await page.click('#playBtn'); // pause
  const t2 = await page.textContent('#tElapsed');
  await page.waitForTimeout(500);
  const t3 = await page.textContent('#tElapsed');
  check('playback advances and pauses', t1 !== '0:00.0' && t2 === t3, `after 1.5 s: ${t1}, paused at ${t3}`);
  // seek by clicking the waveform at 50 %
  const box = await page.locator('#wave').boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  const t4 = await page.textContent('#tElapsed');
  check('waveform click seeks', t4.startsWith('0:04') || t4.startsWith('0:05'), t4);
  await page.click('#playBtn');
  await page.waitForTimeout(700);
  await page.screenshot({ path: path.join(outDir, 'shot_player.png') });
  await page.click('#playBtn');

  // decode back to WAV with the button
  const wavOut = await nextDownload(() => page.click('#decodeBtn'));
  check('decode restores the byte-identical WAV', Buffer.compare(fs.readFileSync(wavOut), fs.readFileSync(wavPath)) === 0);

  // excerpt via command line
  const exPath = await nextDownload(() => command('decode --start-time 2 --duration 1'));
  const exSize = fs.statSync(exPath).size;
  check('command-line excerpt decode', exSize === 48000 * 2 * 4 + 58, `${exSize} bytes`);

  // help
  await command('help');
  check('help lists options', (await page.textContent('#log')).includes('--min-block'));

  // integer PCM input
  const int24 = path.join(outDir, 'int24.wav');
  writeInt24Wav(int24, 44100, 2, 44100 * 2);
  await page.setInputFiles('#fileInput', int24);
  await page.waitForFunction(() => document.getElementById('fileName').textContent === 'int24.wav' && !document.getElementById('playBtn').disabled, null, { timeout: 60000 });
  const hint = await page.textContent('#encHint');
  const nx24 = await nextDownload(() => page.click('#encodeBtn'));
  await page.waitForSelector('#result .badge', { timeout: 120000 });
  check('24-bit integer WAV: exact conversion + bit-exact verify', hint.includes('converted exactly') && (await page.textContent('#result')).includes('bit-exact'), `${fs.statSync(nx24).size} bytes from ${fs.statSync(int24).size}`);

  // broken file
  const junk = path.join(outDir, 'junk.nx');
  fs.writeFileSync(junk, Buffer.from('VFA\x1a this is not really a VFA file'));
  await page.setInputFiles('#fileInput', junk);
  await page.waitForTimeout(500);
  check('corrupt file shows an error, no crash', (await page.textContent('#log')).includes('junk.nx'));

  check('no JavaScript errors', errors.length === 0, errors.join(' | '));
  await browser.close();
  const failed = results.filter((r) => !r.ok).length;
  console.log(`\n${results.length - failed}/${results.length} checks passed`);
  process.exit(failed ? 1 : 0);
})();
