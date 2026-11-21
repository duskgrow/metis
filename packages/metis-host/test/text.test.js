import assert from "node:assert/strict";
import test from "node:test";

import { decodeText, MAX_TEXT_BYTES } from "../lib/text.js";

test("Host text decoding rejects invalid UTF-8", () => {
  assert.throws(() => decodeText(Uint8Array.of(0xff)), TypeError);
});

test("Host text decoding enforces its wire budget", () => {
  assert.throws(
    () => decodeText(new Uint8Array(MAX_TEXT_BYTES + 1)),
    /text exceeds/u,
  );
});
