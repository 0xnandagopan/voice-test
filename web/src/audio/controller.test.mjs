import {test} from 'node:test';
import assert from 'node:assert/strict';
import {AudioController} from './controller.ts';

function fixture() {
  const counts = {mic:0, stopped:0, closed:0, flushed:0, sent:0, terminal:0, played:0};
  const stream = {getTracks:()=>[{stop:()=>counts.stopped++}]};
  const engine = {close:()=>counts.closed++,clearPlayback:()=>counts.flushed++,play:()=>counts.played++};
  let event, audio;
  const deps = {getMicrophone:async()=>{counts.mic++; return stream;},createEngine:async(_,cb)=>{audio=cb;return engine;},playTestTone:async()=>{}};
  const transport = {sendAudio:()=>counts.sent++,stop:()=>counts.terminal++,close:()=>{}};
  const connect=async cb=>{event=cb;return transport;};
  return {counts,stream,engine,deps,connect,event:e=>event(e),audio:b=>audio(b)};
}
test('consent gates readiness and transmission; readiness releases track',async()=>{
  const f=fixture(), c=new AudioController(f.deps);
  await assert.rejects(c.checkMicrophone(false),/consent/);
  await assert.rejects(c.start({consented:false,connect:f.connect}),/consent/);
  assert.equal(f.counts.mic,0);
  await c.checkMicrophone(true); assert.equal(f.counts.stopped,1); assert.equal(c.state,'idle');
});
test('Stop wins a pending microphone permission race',async()=>{
  const f=fixture(); let resolve;
  f.deps.getMicrophone=()=>new Promise(r=>resolve=r);
  const c=new AudioController(f.deps); const started=c.start({consented:true,connect:f.connect});
  await Promise.resolve(); c.stop(); resolve(f.stream); await started;
  assert.equal(c.state,'stopped'); assert.equal(f.counts.stopped,1); assert.equal(f.counts.terminal,1);
});
test('Stop releases local capture when network Stop throws; late frames ignored',async()=>{
  const f=fixture(), c=new AudioController(f.deps);
  await c.start({consented:true,connect:async cb=>{await f.connect(cb); return {sendAudio:()=>f.counts.sent++,stop:()=>{throw Error('offline');},close:()=>{}};}});
  f.audio(new ArrayBuffer(2)); c.stop(); f.audio(new ArrayBuffer(2));
  assert.equal(f.counts.sent,1); assert.equal(f.counts.stopped,1); assert.equal(f.counts.closed,1); assert.equal(c.state,'stopped');
});
test('semantic interruption clears playback while back-channel does not',async()=>{
  const f=fixture(), c=new AudioController(f.deps); await c.start({consented:true,connect:f.connect});
  f.event({type:'input.speech.started'}); assert.equal(f.counts.flushed,0);
  f.event({type:'reply.audio',data:'AAA='}); assert.equal(f.counts.played,1);
  f.event({type:'reply.done',status:'interrupted'}); assert.equal(f.counts.flushed,1);
  f.event({type:'reply.audio',data:'AAA='}); assert.equal(f.counts.played,1);
  f.event({type:'reply.started'}); f.event({type:'reply.audio',data:'AAA='}); assert.equal(f.counts.played,2); c.dispose();
});
test('accidental loss closes local audio without terminal provider end',async()=>{
  const f=fixture(), c=new AudioController(f.deps); await c.start({consented:true,connect:f.connect});
  f.event({type:'disconnect'}); assert.equal(f.counts.terminal,0); assert.equal(f.counts.stopped,1); assert.equal(c.state,'error');
});
test('late provider connection is ended after disposal',async()=>{
  const f=fixture(), c=new AudioController(f.deps); let resolve;
  const started=c.start({consented:true,connect:()=>new Promise(r=>resolve=r)}); c.dispose();
  resolve({stop:()=>f.counts.terminal++,close:()=>{},sendAudio:()=>{}}); await started;
  assert.equal(f.counts.terminal,1); assert.equal(f.counts.mic,0); assert.equal(c.state,'disposed');
});
test('duplicate Start is rejected while connection is pending',async()=>{
  const f=fixture(), c=new AudioController(f.deps); let resolve;
  const pending=c.start({consented:true,connect:()=>new Promise(r=>resolve=r)});
  await assert.rejects(c.start({consented:true,connect:f.connect}),/already active/);
  c.stop(); resolve({stop:()=>{},close:()=>{},sendAudio:()=>{}}); await pending;
  assert.equal(f.counts.mic,0);
});
