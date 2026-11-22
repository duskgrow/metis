import { open, readdir } from "node:fs/promises";
import { join } from "node:path";

async function isTargetElf(path) {
  const handle = await open(path, "r");
  try {
    const header = Buffer.alloc(20);
    const { bytesRead } = await handle.read(header, 0, header.length, 0);
    if (bytesRead < header.length || header[0] !== 0x7f || header.subarray(1, 4).toString() !== "ELF") {
      return false;
    }
    const machine = header[5] === 1
      ? header.readUInt16LE(18)
      : header.readUInt16BE(18);
    const expected = { x64: 62, arm64: 183 }[process.arch];
    if (!expected) throw new Error(`unsupported native architecture: ${process.arch}`);
    const otherPlatform = /(?:^|[\\/])(?:darwin|windows|win32|android)(?:[\\/]|$)/iu.test(path);
    const musl = /(?:^|[\\/])musl[^\\/]*(?:[\\/]|$)|linuxmusl/iu.test(path);
    return machine === expected && !otherPlatform && !musl;
  } finally {
    await handle.close();
  }
}

/** Find native modules and shared libraries matching the packaged glibc ABI. */
export async function nativeFiles(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory() && !entry.isSymbolicLink()) {
      files.push(...await nativeFiles(path));
    } else if (
      entry.isFile()
      && (entry.name.endsWith(".node") || entry.name.includes(".so"))
      && await isTargetElf(path)
    ) {
      files.push(path);
    }
  }
  return files;
}
