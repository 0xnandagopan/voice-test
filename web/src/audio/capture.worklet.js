// Original streaming linear resampler; preserve fractional position across render quanta.
// Voice input contract is little-endian PCM16 mono 24 kHz. No recording is retained here.
class TestimonialPcmCapture extends AudioWorkletProcessor {
  constructor() {
    super(); this.pending = []; this.position = 0; this.frame = new Int16Array(480); this.used = 0;
  }
  process(inputs, outputs) {
    for (const output of outputs) for (const channel of output) channel.fill(0);
    const samples = inputs[0]?.[0];
    if (!samples) return true;
    for (const sample of samples) this.pending.push(sample);
    const step = sampleRate / 24000;
    while (this.position + 1 < this.pending.length) {
      const index = Math.floor(this.position); const fraction = this.position - index;
      const value = Math.max(-1, Math.min(1, this.pending[index] * (1 - fraction) + this.pending[index + 1] * fraction));
      this.frame[this.used++] = Math.round(value < 0 ? value * 32768 : value * 32767);
      this.position += step;
      if (this.used === this.frame.length) {
        // Explicit endian encoding also works on non-little-endian hosts.
        const pcm = new ArrayBuffer(this.frame.length * 2); const data = new DataView(pcm);
        for (let i = 0; i < this.frame.length; i++) data.setInt16(i * 2, this.frame[i], true);
        this.port.postMessage(pcm, [pcm]); this.used = 0;
      }
    }
    const consumed = Math.floor(this.position); this.pending.splice(0, consumed); this.position -= consumed;
    return true;
  }
}
registerProcessor('testimonial-pcm-capture', TestimonialPcmCapture);
