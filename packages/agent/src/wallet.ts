const HEX_RE = /^(0x)?[0-9a-fA-F]+$/;

export function decodeWei(hex: string): bigint {
  if (!HEX_RE.test(hex)) throw new Error(`decodeWei: not a hex string: ${hex}`);
  return BigInt(hex.startsWith("0x") ? hex : `0x${hex}`);
}
