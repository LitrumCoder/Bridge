// RFC 4648 lowercase base32 without padding, as the daemon prints device ids.
const ALPHABET = 'abcdefghijklmnopqrstuvwxyz234567';

export function base32(bytes: number[]): string {
  let out = '';
  let buffer = 0;
  let bits = 0;
  for (const b of bytes) {
    buffer = (buffer << 8) | b;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += ALPHABET[(buffer >> bits) & 31];
    }
    buffer &= (1 << bits) - 1;
  }
  if (bits > 0) {
    out += ALPHABET[(buffer << (5 - bits)) & 31];
  }
  return out;
}

/** The short form shown to people: the first 8 characters. */
export function fingerprint(bytes: number[]): string {
  return base32(bytes).slice(0, 8);
}

export function sameId(a: number[], b: number[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}
