import { createHash } from "node:crypto";
import { writeFile } from "node:fs/promises";
import {
  applyImageDataAdjustments,
  classifyImageStyle,
  ditherImage,
  getProcessingPresetOptions,
  suggestProcessingOptions,
} from "../vendor/epdoptimize/dist/index.mjs";

const OFFLINE_COMMIT = "0961aa69ffeddfe505f7a2bb4b7c45f49308b2d6";
const EPDOPTIMIZE_COMMIT = "555c7a3c42d9233a17b26ec399f3d57340dde8f5";
const palette = [
  { name: "black", color: "#1f2226", deviceColor: "#000000", deviceIndex: 0 },
  { name: "white", color: "#d6d6d6", deviceColor: "#ffffff", deviceIndex: 6 },
  { name: "blue", color: "#416ce1", deviceColor: "#0000ff", deviceIndex: 1 },
  { name: "green", color: "#067406", deviceColor: "#00ff00", deviceIndex: 2 },
  { name: "red", color: "#ea4843", deviceColor: "#ff0000", deviceIndex: 3 },
  { name: "yellow", color: "#dbd529", deviceColor: "#ffff00", deviceIndex: 5 },
];
const rgb = palette.map(entry => hex(entry.color));
const hash = (x, y, seed = 0) => (Math.imul(x + seed + 101, 1103515245) ^ Math.imul(y + 7, 12345)) >>> 0;
const patterns = {
  photo: (x, y, w, h) => { const n=hash(x,y); return [30+x*180/w+n%25,50+y*150/h+(n>>8)%25,180-x*100/w+(n>>16)%25,255]; },
  low_contrast: (x,y,w,h)=>{const n=hash(x,y);const v=110+x*30/w+y*20/h+n%7;return[v+5,v,v-5,255]},
  high_contrast: (x,y,w,h)=>{const n=hash(x,y);const v=x<w/2?20+x*50/w:180+x*70/w;return[v+n%30,v*.8+(n>>8)%30,v*.6+(n>>16)%30,255]},
  flat_illustration: (x,y,w)=>x<w/3?[240,60,40,255]:x<2*w/3?[30,130,220,255]:[240,210,40,255],
  line_art: (x,y)=>(x%20<2||y%20<2)?[10,10,10,255]:[245,245,240,255],
  text_ui: (x,y)=>{let line=y%6;let glyph=x%5<3&&line>2&&line<4;let v=glyph?20:245;if((x+y)%17===0)v=180;return[v,v,v,255]},
  pixel_art: (x,y)=>[[255,0,0,255],[0,0,255,255],[255,255,0,255],[0,255,0,255]][((x>>4)+(y>>4))%4],
  warm_poster: (x,y)=>x>50&&x<110&&y>35&&y<85?[195,25,20,255]:(x%30<4||y%30<4?[25,25,20,255]:[220,200,155,255]),
  grayscale: (x,y,w)=>{const v=clamp(30+x*200/w+(hash(x,y)%20)-10);return[v,v,v,255]},
  transparent: (x,y,w,h)=>[40+x*180/w,80+y*130/h,180,((x+y)%9===0)?0:255],
};

class MemoryCanvas {
  constructor(width, height, data) { this.width=width;this.height=height;this.data=new Uint8ClampedArray(data); }
  getContext() { return { getImageData:()=>({width:this.width,height:this.height,data:new Uint8ClampedArray(this.data)}), putImageData:value=>{this.data.set(value.data)} }; }
}
function resolveLikeGeneratePicture(image) {
  const suggestion=suggestProcessingOptions(image,palette,{intent:"natural"});
  let resolved={...suggestion.ditherOptions};
  if(resolved.processingPreset){const presetValues=getProcessingPresetOptions(resolved.processingPreset);resolved={...presetValues,...resolved};}
  delete resolved.processingPreset;
  // app.js and buildDitherOptions force the selected defaults after auto.
  resolved.errorDiffusionMatrix="floydSteinberg";
  resolved.serpentine=resolved.serpentine??true;
  resolved.ditheringType="errorDiffusion";
  resolved.colorMatching=resolved.colorMatching??"rgb";
  return {suggestion,resolved};
}
async function makeCase(name,fn,width=160,height=120){const data=new Uint8ClampedArray(width*height*4);for(let y=0;y<height;y++)for(let x=0;x<width;x++){let c=fn(x,y,width,height).map(clamp);data.set(c,(y*width+x)*4)}const image={width,height,data:new Uint8ClampedArray(data)};const classification=classifyImageStyle(image);const {suggestion,resolved}=resolveLikeGeneratePicture(image);const adjusted={width,height,data:new Uint8ClampedArray(data)};applyImageDataAdjustments(adjusted,{...resolved,palette});const canvas=new MemoryCanvas(width,height,data);await ditherImage(canvas,canvas,{...resolved,palette});const positions=[];const packed=new Uint8Array(Math.ceil(width*height/2));for(let i=0;i<width*height;i++){const p=i*4;let best=0,dist=Infinity;for(let j=0;j<rgb.length;j++){let d=(canvas.data[p]-rgb[j][0])**2+(canvas.data[p+1]-rgb[j][1])**2+(canvas.data[p+2]-rgb[j][2])**2;if(d<dist){dist=d;best=j}}positions.push(best);const device=palette[best].deviceIndex;if(i%2===0)packed[i>>1]=device<<4;else packed[i>>1]|=device;}return{name,width,height,input_rgba_b64:b64(data),classification:normalizeClassification(classification),raw_suggestion:jsonClean(suggestion.ditherOptions),resolved_options:jsonClean(resolved),adjusted_rgba_b64:b64(adjusted.data),palette_positions_b64:b64(Uint8Array.from(positions)),packed_b64:b64(packed),hashes:{input:sha(data),adjusted:sha(adjusted.data),positions:sha(Uint8Array.from(positions)),packed:sha(packed)}}}
function normalizeClassification(c){return{style:c.style,kind:c.kind,confidence:c.confidence,photoScore:c.photoScore,metrics:c.metrics,kindScores:c.kindScores}}
function jsonClean(v){return JSON.parse(JSON.stringify(v))}
function hex(value){let n=parseInt(value.slice(1),16);return[(n>>16)&255,(n>>8)&255,n&255]}
function clamp(v){return Math.max(0,Math.min(255,Math.round(v??255)))}
function b64(v){return Buffer.from(v).toString("base64")}
function sha(v){return createHash("sha256").update(v).digest("hex")}
const cases=[];for(const[name,fn]of Object.entries(patterns))cases.push(await makeCase(name,fn));
const fixture={schema:1,upstream:{offline_firmware_commit:OFFLINE_COMMIT,epdoptimize_version:"1.2.0",epdoptimize_commit:EPDOPTIMIZE_COMMIT,npm_tarball:"https://registry.npmjs.org/epdoptimize/-/epdoptimize-1.2.0.tgz",npm_integrity:"sha512-0wZbhbP8lEmvei1Maacqn0dCcFCuAIlOZSNYDfRKwl6n9NUlVQz9OA42IE9V2PwB/UsSmFy/y9VBiPUIVMSKmQ=="},oracle:"GeneratePicture.js default spectra6Custom + Auto-Optimieren natural + app/build default overrides; RGBA canvas bypasses browser decoding/resizing",palette,cases};
const path=new URL("../tests/fixtures/generate-picture-auto-v2.json",import.meta.url);await writeFile(path,JSON.stringify(fixture,null,2)+"\n");console.log(`${path.pathname}: ${cases.length} cases ${sha(Buffer.from(JSON.stringify(fixture)))}`);
