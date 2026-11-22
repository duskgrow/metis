import { TextDecoder } from "node:util";

// JSON escaping can expand one input byte to six. Keeping text below this
// bound also keeps its response within the Rust client's 4 MiB envelope limit.
export const MAX_TEXT_BYTES = 512 * 1024;

export function decodeText(bytes: Uint8Array): string {
  if (bytes.byteLength > MAX_TEXT_BYTES) {
    throw new Error(`text exceeds the ${MAX_TEXT_BYTES}-byte limit`);
  }
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
}
