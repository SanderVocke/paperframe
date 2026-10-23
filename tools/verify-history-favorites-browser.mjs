// Destructive only to a new temporary fixture directory. Requires a built server
// and geckodriver listening at WEBDRIVER_URL (default http://127.0.0.1:4444).
// Run: node tools/verify-history-favorites-browser.mjs
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, copyFile, writeFile, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { spawn } from 'node:child_process';

const root = await mkdtemp(join(tmpdir(), 'paperframe-browser-'));
const origin = 'http://127.0.0.1:18549';
const webdriver = process.env.WEBDRIVER_URL || 'http://127.0.0.1:4444';
const source = join(root, 'source');
await mkdir(source); await mkdir(join(root, 'queue'));
for (const name of ['a', 'b', 'c']) await copyFile(resolve(`tests/fixtures/images/${name}.jpg`), join(source, `${name}.jpg`));
await writeFile(join(root, 'config.toml'), `bind = "127.0.0.1:18549"\nsettings_path = "${root}/settings.toml"\nqueue_dir = "${root}/queue"\n`);
await writeFile(join(root, 'settings.toml'), `[queue]\ntarget_length = 3\nmax_upload_bytes = 26214400\n[history]\nmax_entries = 2\n[advancement]\ntype = "manual"\n[[suppliers]]\nid = "primary"\ntype = "manual_folder"\n[suppliers.settings]\ndirectory = "${source}"\npoll_interval_seconds = 60\n`);
let server, session;
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function waitFor(fn, label) { for (let i = 0; i < 200; i++) { if (await fn()) return; await delay(50); } throw Error(`Timed out: ${label}`); }
async function start() {
  server = spawn(resolve(process.env.PAPERFRAME_BINARY || 'target/debug/paperframe-server'), [join(root, 'config.toml')], { stdio: ['ignore', 'pipe', 'pipe'] });
  let logs = ''; server.stdout.on('data', bytes => logs += bytes); server.stderr.on('data', bytes => logs += bytes);
  server.on('error', error => logs += error.message);
  await waitFor(async () => { if (server.exitCode !== null) throw Error(logs); try { return (await fetch(origin + '/health/ready')).ok; } catch { return false; } }, 'server ready');
}
async function stop() { if (!server || server.exitCode !== null) return; const exited = new Promise(resolve => server.once('exit', resolve)); server.kill('SIGTERM'); await exited; }
async function command(method, path, body) {
  const response = await fetch(webdriver + path, { method, headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  const result = await response.json();
  if (!response.ok || result.value?.error) throw Error(JSON.stringify(result));
  return result.value;
}
const execute = script => command('POST', `/session/${session}/execute/sync`, { script, args: [] });
async function navigate(path) { await command('POST', `/session/${session}/url`, { url: origin + path }); }
async function click(selector) { await execute(`document.querySelector(${JSON.stringify(selector)}).click();`); await delay(250); }
async function screenshot(name) { const image = await command('GET', `/session/${session}/screenshot`); await writeFile(join(root, name + '.png'), Buffer.from(image, 'base64')); }
async function assertLayout(label) {
  await execute('document.querySelectorAll("img").forEach(image=>image.loading="eager");');
  await waitFor(() => execute('return [...document.images].every(image=>image.complete);'), 'images loaded');
  assert(await execute('return document.documentElement.scrollWidth <= innerWidth + 1;'), `${label}: horizontal overflow`);
  assert(await execute('return [...document.images].filter(image=>image.src).every(image=>image.naturalWidth>0);'), `${label}: broken image`);
  await screenshot(label);
}
try {
  await start();
  const options = { args: ['-headless'] };
  if (process.env.FIREFOX_BINARY) options.binary = process.env.FIREFOX_BINARY;
  session = (await command('POST', '/session', { capabilities: { alwaysMatch: { browserName: 'firefox', 'moz:firefoxOptions': options } } })).sessionId;
  await command('POST', `/session/${session}/window/rect`, { width: 1280, height: 1000 });
  await navigate('/settings');
  await assertLayout('desktop-queue');
  await click('form[action="/favorites/add"] button');
  await navigate('/history');
  await click('[data-preview-trigger]');
  assert(await execute('return document.querySelector("#photo-preview").open;'));
  assert(await execute('return document.querySelector("#original-preview").src.includes("/saved/preview/");'));
  await assertLayout('desktop-preview');
  await click('#preview-close');
  await navigate('/favorites');
  await execute('document.querySelector("input[name=path]").value="b.jpg";');
  await click('form[action="/favorites/path"] button');
  assert.equal(await execute('return document.querySelectorAll("article").length;'), 2);
  const element = await command('POST', `/session/${session}/element`, { using: 'css selector', value: 'input[type=file]' });
  const elementId = element['element-6066-11e4-a52e-4f735466cecf'];
  await command('POST', `/session/${session}/element/${elementId}/value`, { text: join(source, 'c.jpg') });
  await click('form[action="/favorites/upload"] button');
  assert.equal(await execute('return document.querySelectorAll("article").length;'), 3);
  await assertLayout('desktop-favorites');
  await execute(`document.addEventListener('submit', event=>sessionStorage.setItem('pending-proof', JSON.stringify({disabled:event.submitter.disabled,pending:event.target.dataset.pending,position:event.target.querySelector('input[name=position]').value})), {once:true});`);
  await click('form[action="/queue/from-saved"] button[value=front]');
  assert.deepEqual(JSON.parse(await execute("return sessionStorage.getItem('pending-proof');")), { disabled: true, pending: 'true', position: 'front' });
  assert.equal(await execute('return document.querySelectorAll(".queue>li").length;'), 4);
  assert.equal(await execute('return document.querySelector(".queue .photo-name").textContent;'), 'c.jpg');
  await navigate('/favorites');
  await click('form[action="/queue/from-saved"] button[value=back]');
  assert.equal(await execute('return document.querySelectorAll(".queue>li").length;'), 5);
  await click('form[action="/queue/advance"] button');
  await navigate('/history');
  assert.equal(await execute('return document.querySelectorAll("article").length;'), 2);
  await command('POST', `/session/${session}/window/rect`, { width: 390, height: 844 });
  for (const [path, name] of [['/settings', 'mobile-queue'], ['/history', 'mobile-history'], ['/favorites', 'mobile-favorites']]) { await navigate(path); await assertLayout(name); }
  await execute('document.querySelector(\'article form[action="/queue/from-saved"]\').scrollIntoView({block:"center",behavior:"instant"});');
  await waitFor(()=>execute('const rect=document.querySelector(\'article form[action="/queue/from-saved"]\').getBoundingClientRect();return rect.top>=0 && rect.bottom<=innerHeight;'), 'mobile card actions visible');
  await delay(300);
  await screenshot('mobile-favorite-actions');
  const cachedPaths = await execute(`return navigator.serviceWorker.ready.then(()=>fetch('/manifest.webmanifest')).then(()=>caches.keys()).then(names=>Promise.all(names.map(name=>caches.open(name).then(cache=>cache.keys())))).then(groups=>groups.flat().map(request=>new URL(request.url).pathname));`);
  assert(cachedPaths.includes('/manifest.webmanifest'), 'PWA metadata must actually exercise the cache');
  assert(cachedPaths.every(path=>['/manifest.webmanifest','/pwa/icon-192.png','/pwa/icon-512.png','/pwa/icon-maskable-512.png','/pwa/inter-variable.ttf'].includes(path)), 'private pages/photos entered service-worker cache');
  const viewport = await execute('return {width:innerWidth,height:innerHeight};');
  assert(viewport.width <= 680, 'narrow layout must hit mobile breakpoint');
  await stop(); await start();
  await navigate('/favorites');
  assert.equal(await execute('return document.querySelectorAll("article").length;'), 3);
  await navigate('/history');
  assert.equal(await execute('return document.querySelectorAll("article").length;'), 2);
  const descriptor = await readFile(join(root, 'queue/queue-state.toml'), 'utf8');
  assert(descriptor.includes('history_limit = 2'));
  await writeFile(join(root, 'report.json'), JSON.stringify({ passed: true, browser: 'Firefox WebDriver', checks: ['desktop/mobile layout', 'loaded thumbnails', 'shared saved preview', 'queue favorite', 'server path import', 'client upload', 'front/back insertion', 'pending submit/position preservation', 'history trimming', 'PWA private-cache exclusion', 'restart'], viewport, root }, null, 2));
  console.log(`PASS: browser evidence in ${root}`);
} finally {
  if (session) await command('DELETE', `/session/${session}`).catch(() => {});
  await stop();
}
