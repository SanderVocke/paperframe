import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { deflateSync } from "node:zlib";
import { chromium } from "playwright";

const root = resolve(new URL("../../", import.meta.url).pathname);
const offline = process.env.OFFLINE_FIRMWARE_DIR;
const executablePath = process.env.CHROMIUM_PATH;
if (!offline || !executablePath) {
  throw new Error("Set OFFLINE_FIRMWARE_DIR to commit 0961aa69 and CHROMIUM_PATH to pinned Chromium");
}
const output = resolve(root, "tests/fixtures/browser");
await mkdir(output, { recursive: true });
await mkdir("/tmp/paperframe-chromium-home", { recursive: true });
const fixtures = [
  { name: "paper7", display: "7", width: 800, height: 480, sourceWidth: 800, sourceHeight: 480 },
  { name: "paperl", display: "13", width: 1200, height: 1600, sourceWidth: 1200, sourceHeight: 1600 },
  { name: "paper7-resized", display: "7", width: 800, height: 480, sourceWidth: 613, sourceHeight: 409 },
  { name: "paperl-rotated-resized", display: "13", width: 1200, height: 1600, sourceWidth: 777, sourceHeight: 511 },
];
for (const fixture of fixtures) {
  const rgba = new Uint8Array(fixture.sourceWidth * fixture.sourceHeight * 4);
  for (let y=0;y<fixture.sourceHeight;y++) for (let x=0;x<fixture.sourceWidth;x++) {
    const i=(y*fixture.sourceWidth+x)*4;
    const noise=(Math.imul(x+101,1103515245)^Math.imul(y+7,12345))>>>0;
    rgba[i]=Math.round(30+180*x/Math.max(1,fixture.sourceWidth-1)+noise%25);
    rgba[i+1]=Math.round(50+150*y/Math.max(1,fixture.sourceHeight-1)+(noise>>8)%25);
    rgba[i+2]=Math.round(180-100*x/Math.max(1,fixture.sourceWidth-1)+(noise>>16)%25);
    rgba[i+3]=255;
  }
  await writeFile(resolve(output, `${fixture.name}-source.png`), encodePng(fixture.sourceWidth, fixture.sourceHeight, rgba));
}

const server=spawn("npm",["run","dev","--","--host","127.0.0.1","--port","41739"],{cwd:resolve(offline,"web"),stdio:["ignore","pipe","inherit"]});
try {
  await waitForServer(server);
  await new Promise(resolve=>setTimeout(resolve,1000));
  const browser=await chromium.launch({headless:true,executablePath,args:["--no-sandbox","--disable-crash-reporter"],env:{...process.env,HOME:"/tmp/paperframe-chromium-home",XDG_CONFIG_HOME:"/tmp/paperframe-chromium-home/config",XDG_CACHE_HOME:"/tmp/paperframe-chromium-home/cache"}});
  const page=await browser.newPage({acceptDownloads:true});
  page.on("console",message=>console.log(`browser: ${message.type()} ${message.text()}`));
  page.on("pageerror",error=>console.error(`browser page error: ${error.message}`));
  const results=[];
  for(const fixture of fixtures){
    await page.goto("http://127.0.0.1:41739/",{waitUntil:"networkidle"});
    await page.evaluate(()=>document.querySelector("#viewEditor").classList.remove("hidden"));
    await page.selectOption("#displaySizeSelect",fixture.display,{force:true});
    await page.setInputFiles("#fileInput",resolve(output,`${fixture.name}-source.png`));
    await page.waitForFunction(()=>!document.querySelector("#btnDownloadBin").disabled,{timeout:120000});
    const oracleInfo=await page.evaluate(async({width,height})=>{
      const {GeneratePicture}=await import("/GeneratePicture.js");
      const file=document.querySelector("#fileInput").files[0];
      const image=new Image();image.src=URL.createObjectURL(file);await image.decode();
      const generator=new GeneratePicture();generator.setEpdDimensions(width,height);generator.setOriginalImage(image);
      if((image.height>image.width&&width>height)||(image.width>image.height&&height>width))generator.setImageRotation(270);
      const canvas=document.createElement("canvas");canvas.width=width===1200?600:width;canvas.height=height===1600?800:height;
      const result=generator.autoOptimise(canvas,"spectra6Custom");
      URL.revokeObjectURL(image.src);
      return {classification:result.suggestion?.classification??result.classification,imageKind:result.suggestion?.imageKind??result.imageKind,rawOptions:result.resolvedOptions??result};
    },{width:fixture.width,height:fixture.height});
    await page.click("#btnAutoDither",{force:true});
    await page.waitForFunction(()=>!document.querySelector("#btnDownloadBin").disabled,{timeout:120000});
    await page.waitForTimeout(500);
    const downloadPromise=page.waitForEvent("download",{timeout:60000});
    await page.evaluate(()=>document.querySelector("#btnDownloadBin").click());
    let download;
    try { download=await downloadPromise; }
    catch(error) { console.error(`download failed for ${fixture.name}: ${await page.locator("#statusText").textContent()}`); throw error; }
    const bmp=resolve(output,`${fixture.name}-auto.bmp`);
    await download.saveAs(bmp);
    results.push({name:fixture.name,width:fixture.width,height:fixture.height,source_width:fixture.sourceWidth,source_height:fixture.sourceHeight,source_sha256:sha(await readFile(resolve(output,`${fixture.name}-source.png`))),bmp_sha256:sha(await readFile(bmp)),classification_kind:oracleInfo.imageKind,classification:oracleInfo.classification,raw_auto_options:oracleInfo.rawOptions});
  }
  const version=await browser.version();
  await browser.close();
  await writeFile(resolve(output,"metadata.json"),JSON.stringify({schema:1,oracle:"actual GeneratePicture.js UI: defaults, Auto-Optimieren, download",offline_firmware_commit:"0961aa69ffeddfe505f7a2bb4b7c45f49308b2d6",epdoptimize_version:"1.2.0",chromium:version,fixtures:results},null,2)+"\n");
  console.log(`generated ${results.length} browser fixtures with ${version}`);
} finally { server.kill("SIGTERM"); }

function waitForServer(server){return new Promise((ok,fail)=>{let text="";let timer=setTimeout(()=>fail(new Error("Vite did not start")),60000);server.stdout.on("data",chunk=>{text+=chunk;if(text.includes("41739")){clearTimeout(timer);ok();}});server.on("exit",code=>fail(new Error(`Vite exited ${code}`)));});}
function sha(data){return createHash("sha256").update(data).digest("hex")}
function encodePng(width,height,rgba){const rows=Buffer.alloc((width*4+1)*height);for(let y=0;y<height;y++){const at=y*(width*4+1);rows[at]=0;Buffer.from(rgba.buffer,rgba.byteOffset+y*width*4,width*4).copy(rows,at+1);}const signature=Buffer.from([137,80,78,71,13,10,26,10]);const ihdr=Buffer.alloc(13);ihdr.writeUInt32BE(width,0);ihdr.writeUInt32BE(height,4);ihdr[8]=8;ihdr[9]=6;return Buffer.concat([signature,chunk("IHDR",ihdr),chunk("IDAT",deflateSync(rows,{level:9})),chunk("IEND",Buffer.alloc(0))]);}
function chunk(type,data){const name=Buffer.from(type);const out=Buffer.alloc(data.length+12);out.writeUInt32BE(data.length,0);name.copy(out,4);data.copy(out,8);out.writeUInt32BE(crc32(Buffer.concat([name,data])),8+data.length);return out;}
function crc32(data){let c=0xffffffff;for(const byte of data){c^=byte;for(let k=0;k<8;k++)c=(c>>>1)^((c&1)?0xedb88320:0);}return(c^0xffffffff)>>>0;}
