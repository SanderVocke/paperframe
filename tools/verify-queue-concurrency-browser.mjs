// Uses only a fresh temporary fixture. Requires a built server and geckodriver.
// Run: node tools/verify-queue-concurrency-browser.mjs
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, copyFile, writeFile, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { spawn } from 'node:child_process';

const root=await mkdtemp(join(tmpdir(),'paperframe-queue-browser-'));
const source=join(root,'source'),origin='http://127.0.0.1:18550';
const webdriver=process.env.WEBDRIVER_URL||'http://127.0.0.1:4444';
await mkdir(source);await mkdir(join(root,'queue'));
for(const name of ['a','b','c'])await copyFile(resolve(`tests/fixtures/images/${name}.jpg`),join(source,`${name}.jpg`));
await writeFile(join(root,'config.toml'),`bind = "127.0.0.1:18550"\nsettings_path = "${root}/settings.toml"\nqueue_dir = "${root}/queue"\n`);
await writeFile(join(root,'settings.toml'),`[queue]\ntarget_length = 3\nmax_upload_bytes = 26214400\n[advancement]\ntype = "manual"\n[[suppliers]]\nid = "primary"\ntype = "manual_folder"\n[suppliers.settings]\ndirectory = "${source}"\npoll_interval_seconds = 60\n`);
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function waitFor(fn,label){for(let i=0;i<400;i++){if(await fn())return;await delay(50);}throw Error(`Timed out: ${label}`);}
let session,server;
async function command(method,path,body){
  const response=await fetch(webdriver+path,{method,headers:{'Content-Type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)});
  const result=await response.json();if(!response.ok||result.value?.error)throw Error(JSON.stringify(result));return result.value;
}
const execute=script=>command('POST',`/session/${session}/execute/sync`,{script,args:[]});
const uiIds=()=>execute('return [...document.querySelectorAll("#queue-list>li")].map(card=>card.dataset.id);');
async function snapshot(){
  const html=await(await fetch(origin+'/settings')).text();
  return {html,csrf:html.match(/name="csrf_token" value="([^"]+)"/)[1],generation:html.match(/name="queue_generation" value="([^"]+)"/)[1],ids:[...html.matchAll(/<li class="photo-card[^"]*"[^>]*data-id="([^"]+)"/g)].map(match=>match[1])};
}
async function post(path,fields,base=undefined){
  base??=await snapshot();
  return fetch(origin+path,{method:'POST',body:new URLSearchParams({csrf_token:base.csrf,queue_generation:base.generation,...fields}),redirect:'manual'});
}
async function append(name){
  const base=await snapshot(),data=new FormData();
  data.append('csrf_token',base.csrf);data.append('queue_generation',base.generation);data.append('position','back');
  data.append('images',new Blob([await readFile(join(source,'a.jpg'))],{type:'image/jpeg'}),name);
  assert.equal((await fetch(origin+'/queue/upload',{method:'POST',body:data,redirect:'manual'})).status,303);
}
async function click(selector){await execute(`document.querySelector(${JSON.stringify(selector)}).click();`);}
async function idle(){await waitFor(()=>execute('return ![...document.querySelectorAll("#queue-panel form button,#entry-editor button")].some(button=>button.disabled);'),'action completed');}
async function sameQueue(){await waitFor(async()=>JSON.stringify(await uiIds())===JSON.stringify((await snapshot()).ids),'live queue synchronized');}
async function saveOrder(){await click('#order-form button');await idle();}
try{
  server=spawn(resolve(process.env.PAPERFRAME_BINARY||'target/debug/paperframe-server'),[join(root,'config.toml')],{stdio:['ignore','pipe','pipe']});
  let logs='';server.stdout.on('data',bytes=>logs+=bytes);server.stderr.on('data',bytes=>logs+=bytes);server.on('error',error=>logs+=error.message);
  await waitFor(async()=>{if(server.exitCode!==null)throw Error(logs);try{return(await fetch(origin+'/health/ready')).ok;}catch{return false;}},'server ready');
  const options={args:['-headless']};if(process.env.FIREFOX_BINARY)options.binary=process.env.FIREFOX_BINARY;
  session=(await command('POST','/session',{capabilities:{alwaysMatch:{browserName:'firefox','moz:firefoxOptions':options}}})).sessionId;
  await command('POST',`/session/${session}/window/rect`,{width:1280,height:1000});
  await command('POST',`/session/${session}/url`,{url:origin+'/settings'});
  await execute('window.queueTestMarker=123;window.confirm=()=>true;');
  await sameQueue();

  // Consecutive deletes, with a refill after each, never navigate the page.
  for(const name of ['d','e']){
    await copyFile(join(source,'a.jpg'),join(source,`${name}.jpg`));
    const removed=(await uiIds())[0];
    await click('form[action="/queue/remove"] button');
    await waitFor(async()=>{const queue=await snapshot();return queue.ids.length===3&&!queue.ids.includes(removed);},'delete and refill');
    await sameQueue();
    assert.equal(await execute('return window.queueTestMarker;'),123);
  }
  // Preserve other forms throughout queue updates.
  await execute('document.querySelector("#target_length").value="9";document.querySelector("#message-body").value="unsaved message";');
  await click('#queue-list>li:nth-child(2) [data-move="up"]');
  const draft=await uiIds();
  const baseline=await execute('return document.querySelector("#order-form [name=original_order]").value;');
  await append('background.jpg');
  await delay(5500);
  assert.deepEqual(await uiIds(),draft,'polling replaced unsaved reorder');
  assert.equal(await execute('return document.querySelector("#order-form [name=original_order]").value;'),baseline,'polling silently rebased order');
  const appended=(await snapshot()).ids.at(-1);
  await saveOrder();await sameQueue();
  assert.deepEqual(await uiIds(),[...draft,appended],'new append must stay at the end');

  // A competing reorder yields a friendly conflict, preserving the draft.
  await click('#queue-list>li:nth-child(2) [data-move="up"]');
  const conflictingDraft=await uiIds(),before=await snapshot(),external=[...before.ids];
  [external[1],external[2]]=[external[2],external[1]];
  assert.equal((await post('/queue/reorder',{original_order:before.ids.join('\n'),order:external.join('\n')},before)).status,303);
  await saveOrder();
  assert.match(await execute('return document.querySelector("#queue-feedback").textContent;'),/Action not applied/);
  assert.deepEqual(await uiIds(),conflictingDraft);
  assert.deepEqual((await snapshot()).ids,external,'conflicting save overwrote another client');
  await click('#queue-review');await sameQueue();
  assert.equal(await execute('return document.querySelector("#target_length").value;'),'9');
  assert.equal(await execute('return document.querySelector("#message-body").value;'),'unsaved message');

  // An open entry editor survives polling and can save across a background append.
  await click('[data-edit-trigger]');
  const editing=await execute('return document.querySelector("#entry-editor-id").value;');
  await execute('document.querySelector("#entry-editor-body").value="my caption";');
  await append('while-editing.jpg');await delay(5500);
  assert.equal(await execute('return document.querySelector("#entry-editor-body").value;'),'my caption');
  await click('#entry-editor button[type=submit]');
  await waitFor(()=>execute('return !document.querySelector("#entry-editor").open;'),'edit saved');
  await sameQueue();
  assert.equal(await execute(`return document.querySelector('[data-edit-trigger][data-id="${editing}"]').dataset.text;`),'my caption');

  // A competing edit cannot be overwritten; draft and error stay in the dialog.
  await click(`[data-edit-trigger][data-id="${editing}"]`);
  await execute('document.querySelector("#entry-editor-body").value="local unsaved caption";');
  const version=await execute('return document.querySelector("#entry-editor [name=entry_version]").value;');
  assert.equal((await post('/queue/orientation',{id:editing,entry_version:version,orientation:'portrait'})).status,303);
  await click('#entry-editor button[type=submit]');await idle();
  assert.equal(await execute('return document.querySelector("#entry-editor-body").value;'),'local unsaved caption');
  assert.match(await execute('return document.querySelector("#entry-editor [data-action-feedback]").textContent;'),/edited elsewhere/);
  await click('#entry-editor-close');await click('#queue-review');await sameQueue();

  // Backend advancement rejects a replay instead of skipping another photo.
  const advanceBase=await snapshot(),advanceFields={original_order:advanceBase.ids.join('\n')};
  assert.equal((await post('/queue/advance',advanceFields,advanceBase)).status,303);
  const advanced=(await snapshot()).ids;
  assert.equal((await post('/queue/advance',advanceFields,advanceBase)).status,409);
  assert.deepEqual((await snapshot()).ids,advanced);
  await sameQueue();

  // A delayed pre-mutation poll must not restore deleted cards or stale tokens.
  await execute(`
    window.realQueueFetch=window.fetch;
    let delayed=false;
    window.fetch=async (...args)=>{
      const response=await window.realQueueFetch(...args);
      if(args[0]==='/settings'&&!delayed){delayed=true;return new Promise(resolve=>window.releaseQueuePoll=()=>resolve(response));}
      return response;
    };
    window.dispatchEvent(new Event('pageshow'));
  `);
  await waitFor(()=>execute('return typeof window.releaseQueuePoll==="function";'),'poll held');
  await click('form[action="/queue/remove"] button');await idle();
  const afterDelete=await uiIds();
  await execute('window.releaseQueuePoll();window.fetch=window.realQueueFetch;');await delay(200);
  assert.deepEqual(await uiIds(),afterDelete,'old poll overwrote successful deletion');
  await sameQueue();

  // A 200 login page from an authenticating proxy is not a successful mutation.
  await execute(`
    window.realQueueFetch=window.fetch;
    window.fetch=(...args)=>String(args[0]).endsWith('/queue/message')
      ?Promise.resolve(new Response('<html><body>Sign in</body></html>',{status:200}))
      :window.realQueueFetch(...args);
  `);
  await click('form[action="/queue/message"] button[value=back]');
  await waitFor(()=>execute('return document.querySelector("#queue-feedback").textContent.includes("Could not confirm");'),'uncertain action feedback');
  assert.equal(await execute('return document.querySelector("#message-body").value;'),'unsaved message');
  await execute('window.fetch=window.realQueueFetch;');

  // Empty/non-empty transitions keep dynamically inserted controls functional.
  while((await uiIds()).length){await click('form[action="/queue/remove"] button');await idle();await sameQueue();}
  assert(await execute(`return document.querySelector('form[action="/queue/advance"] button').disabled;`));
  await execute('window.dispatchEvent(new Event("pageshow"));');
  assert(await execute(`return document.querySelector('form[action="/queue/advance"] button').disabled;`));
  const file=await command('POST',`/session/${session}/element`,{using:'css selector',value:'#images'});
  await command('POST',`/session/${session}/element/${file['element-6066-11e4-a52e-4f735466cecf']}/value`,{text:join(source,'a.jpg')});
  await click('form[action="/queue/upload"] button[value=back]');
  await waitFor(async()=> (await uiIds()).length===1,'upload to empty queue');
  await click('[data-preview-trigger]');
  assert(await execute('return document.querySelector("#photo-preview").open;'));
  await click('#preview-close');
  assert.equal(await execute('return window.queueTestMarker;'),123,'queue actions reloaded the document');
  assert.equal(await execute('return document.querySelector("#target_length").value;'),'9');
  assert.equal(await execute('return document.querySelector("#message-body").value;'),'unsaved message');
  const screenshot=await command('GET',`/session/${session}/screenshot`);
  await writeFile(join(root,'queue.png'),Buffer.from(screenshot,'base64'));
  await writeFile(join(root,'report.json'),JSON.stringify({passed:true,root,checks:['delete/refill/delete','delete/reorder','append-aware reorder','competing reorder','entry-scoped editing','competing edit','advance replay protection','draft preservation','empty queue/upload','delegated preview controls','stale poll rejection','authentication redirect preserves draft','no document reload']},null,2));
  console.log(`PASS: queue browser evidence in ${root}`);
}finally{
  if(session)await command('DELETE',`/session/${session}`).catch(()=>{});
  if(server&&server.exitCode===null){const exited=new Promise(resolve=>server.once('exit',resolve));server.kill('SIGTERM');await exited;}
}
