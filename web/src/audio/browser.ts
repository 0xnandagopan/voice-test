import type { AudioDependencies, AudioEngine } from './controller';

export function browserAudioDependencies(): AudioDependencies {
  return {
    getMicrophone: () => navigator.mediaDevices.getUserMedia({audio: {channelCount: 1, echoCancellation: true, noiseSuppression: false}, video: false}),
    async createEngine(stream, onPcm): Promise<AudioEngine> {
      const context = new AudioContext();
      const sources = new Set<AudioBufferSourceNode>();
      let nextTime = 0;
      let capture: AudioWorkletNode | undefined;
      let input: MediaStreamAudioSourceNode | undefined;
      let closed = false;
      const clearPlayback = () => {for (const source of sources) {source.stop(); source.disconnect();} sources.clear(); nextTime = 0;};
      const close = () => {if (closed) return; closed = true; clearPlayback(); if (capture) {capture.port.onmessage = null; capture.disconnect();} input?.disconnect(); void context.close();};
      try {
        await context.audioWorklet.addModule(new URL('./capture.worklet.js', import.meta.url).href);
        if (!stream.getTracks().some(track => track.readyState === 'live')) {close(); throw new Error('Microphone stopped.');}
        input = context.createMediaStreamSource(stream);
        capture = new AudioWorkletNode(context, 'testimonial-pcm-capture', {numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1]});
        capture.port.onmessage = (event: MessageEvent<ArrayBuffer>) => {if (!closed) onPcm(event.data);};
        input.connect(capture); capture.connect(context.destination); // processor emits silence, avoiding feedback
        await context.resume();
      } catch (error) {close(); throw error;}
      return {clearPlayback, close, play(pcm) {
        if (closed) return;
        const view = new DataView(pcm);
        const buffer = context.createBuffer(1, pcm.byteLength / 2, 24_000);
        const samples = buffer.getChannelData(0);
        for (let i = 0; i < samples.length; i++) samples[i] = view.getInt16(i * 2, true) / 32768;
        const source = context.createBufferSource(); source.buffer = buffer; source.connect(context.destination);
        source.onended = () => {sources.delete(source); source.disconnect();};
        sources.add(source); const at = Math.max(context.currentTime, nextTime); source.start(at); nextTime = at + buffer.duration;
      }};
    },
    async playTestTone() {
      const context = new AudioContext();
      try {
        await context.resume();
        const tone = context.createOscillator(); const gain = context.createGain();
        gain.gain.value = 0.08; tone.frequency.value = 440; tone.connect(gain); gain.connect(context.destination);
        await new Promise<void>(resolve => {tone.onended = () => resolve(); tone.start(); tone.stop(context.currentTime + 0.25);});
      } finally {await context.close();}
    },
  };
}
