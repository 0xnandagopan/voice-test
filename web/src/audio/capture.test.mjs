import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
const source=readFileSync(new URL('./capture.worklet.js',import.meta.url),'utf8');
for (const rate of [44100,48000,24000]) test(`worklet preserves stream duration at ${rate}Hz`,()=>{
  const frames=[]; let Processor;
  runInNewContext(source,{sampleRate:rate, AudioWorkletProcessor:class {port={postMessage:pcm=>frames.push(pcm)};},registerProcessor:(_,p)=>Processor=p});
  const p=new Processor();
  for (let offset=0;offset<rate;offset+=128) p.process([[new Float32Array(Math.min(128,rate-offset)).fill(0.5)]],[[new Float32Array(128)]]);
  const samples=frames.reduce((n,b)=>n+b.byteLength/2,0);
  assert.ok(samples>=23520 && samples<=24000); assert.ok(p.pending.length<=2);
  assert.equal(new DataView(frames[0]).getInt16(0,true),16384);
});
