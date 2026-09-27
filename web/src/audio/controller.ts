export type AudioState = 'idle' | 'checking' | 'connecting' | 'active' | 'stopped' | 'error' | 'disposed';
export type VoiceEvent = { type: string; data?: string; status?: string; interrupted?: boolean };
export interface VoiceTransport { sendAudio(pcm: ArrayBuffer): void; stop(): void; close(): void }
export interface AudioEngine { play(pcm: ArrayBuffer): void; clearPlayback(): void; close(): void }
export interface AudioDependencies {
  getMicrophone(): Promise<MediaStream>;
  createEngine(stream: MediaStream, onPcm: (pcm: ArrayBuffer) => void): Promise<AudioEngine>;
  playTestTone(): Promise<void>;
}
/** Independent of React. Each async step is fenced so Stop wins pending permission/connect races. */
export class AudioController {
  state: AudioState = 'idle';
  private generation = 0;
  private stream?: MediaStream;
  private engine?: AudioEngine;
  private transport?: VoiceTransport;
  private suppressPlayback = false;
  private readonly deps: AudioDependencies;
  private readonly onState?: (state: AudioState) => void;
  constructor(deps: AudioDependencies, onState?: (state: AudioState) => void) {this.deps = deps; this.onState = onState;}
  private setState(state: AudioState) { this.state = state; this.onState?.(state); }
  private begin(consented: boolean, state: AudioState) {
    if (!consented) throw new Error('Recording consent is required.');
    if (this.state === 'disposed') throw new Error('Audio controller was disposed.');
    if (['checking', 'connecting', 'active'].includes(this.state)) throw new Error('Audio is already active.');
    const generation = ++this.generation; this.setState(state); return generation;
  }
  private releaseStream(stream?: MediaStream) { stream?.getTracks().forEach(track => track.stop()); }
  async checkMicrophone(consented: boolean): Promise<void> {
    const generation = this.begin(consented, 'checking');
    try {
      const stream = await this.deps.getMicrophone();
      this.releaseStream(stream);
      if (generation === this.generation) this.setState('idle');
    } catch {
      if (generation === this.generation) {this.setState('error'); throw new Error('Microphone access failed. Check browser permission and retry.');}
    }
  }
  async playTestTone(): Promise<void> {
    if (this.state === 'disposed') throw new Error('Audio controller was disposed.');
    await this.deps.playTestTone();
  }
  async start(options: {consented: boolean; connect: (onEvent: (event: VoiceEvent) => void) => Promise<VoiceTransport>}): Promise<void> {
    const generation = this.begin(options.consented, 'connecting');
    this.suppressPlayback = false;
    try {
      // Application relay resolves only after server consent/lease check and durable provider mapping.
      const transport = await options.connect(event => {if (generation === this.generation) this.event(event);});
      if (generation !== this.generation) {try {transport.stop();} finally {transport.close();} return;}
      this.transport = transport;
      const stream = await this.deps.getMicrophone();
      if (generation !== this.generation) {this.releaseStream(stream); return;}
      this.stream = stream;
      const engine = await this.deps.createEngine(stream, pcm => {
        if (generation !== this.generation || this.state !== 'active') return;
        try {this.transport?.sendAudio(pcm);} catch {this.stop(); this.setState('error');}
      });
      if (generation !== this.generation) {engine.close(); return;}
      this.engine = engine; this.setState('active');
    } catch {
      if (generation === this.generation) {this.stop(); this.setState('error'); throw new Error('Voice connection failed. Your microphone has been released.');}
    }
  }
  private event(event: VoiceEvent) {
    if (event.type === 'session.ended') {this.stop(); return;}
    if (event.type === 'disconnect' || event.type === 'session.error') {this.release(false); this.setState('error'); return;}
    // Do not flush on input.speech.started: semantic back-channels are not barge-in.
    if ((event.type === 'reply.done' && event.status === 'interrupted') || (event.type === 'transcript.agent' && event.interrupted)) {
      this.suppressPlayback = true; this.engine?.clearPlayback();
    }
    if (event.type === 'reply.started') this.suppressPlayback = false;
    if (event.type === 'reply.audio' && event.data && !this.suppressPlayback) {
      try {
        const binary = atob(event.data); const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
        if (bytes.length % 2) throw new Error('Invalid PCM');
        this.engine?.play(bytes.buffer);
      } catch {this.stop(); this.setState('error');}
    }
  }
  private release(terminal: boolean) {
    ++this.generation;
    this.releaseStream(this.stream); this.stream = undefined;
    const engine = this.engine; this.engine = undefined;
    try {engine?.clearPlayback();} catch { /* Continue releasing resources. */ }
    try {engine?.close();} catch { /* Tracks were already stopped. */ }
    const transport = this.transport; this.transport = undefined;
    try {if (terminal) transport?.stop();} catch { /* Stop is effective locally even offline. */ }
    try {transport?.close();} catch { /* No network dependency for local teardown. */ }
  }
  stop(): void {if (this.state === 'disposed') return; this.release(true); this.setState('stopped');}
  dispose(): void {if (this.state === 'disposed') return; this.release(true); this.setState('disposed');}
}
