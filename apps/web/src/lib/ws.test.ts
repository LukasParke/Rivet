import { describe, expect, it } from 'vitest';
import { nextBackoffMs } from './ws';

describe('nextBackoffMs', () => {
  it('starts at the base delay and grows exponentially', () => {
    const r = () => 1; // max jitter
    expect(nextBackoffMs(0, r)).toBe(250);
    expect(nextBackoffMs(1, r)).toBe(500);
    expect(nextBackoffMs(2, r)).toBe(1000);
    expect(nextBackoffMs(3, r)).toBe(2000);
  });

  it('caps at 8 seconds', () => {
    const r = () => 1;
    for (let attempt = 10; attempt < 20; attempt++) {
      expect(nextBackoffMs(attempt, r)).toBeLessThanOrEqual(8000);
      expect(nextBackoffMs(attempt, r)).toBeGreaterThan(4000); // [cap/2, cap]
    }
  });

  it('jitter keeps values within [base/2, base]', () => {
    const r = () => 0; // min jitter
    expect(nextBackoffMs(2, r)).toBeGreaterThanOrEqual(500);
    expect(nextBackoffMs(2, r)).toBeLessThanOrEqual(1000);
  });
});
