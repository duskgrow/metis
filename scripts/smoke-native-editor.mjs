import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const workspace = fileURLToPath(new URL("..", import.meta.url));
const platform = process.env.METIS_NATIVE_PLATFORM
  ?? (existsSync("/etc/NIXOS") ? "nixos" : "linux");
assert.ok(
  ["linux", "nixos", "ubuntu-24.04"].includes(platform),
  `unsupported native smoke platform: ${platform}`,
);
const artifacts = join(workspace, "target", "native-editor-smoke", platform);
const clipboardText = "剪贴板中文";
const expectedText = `你好${clipboardText}`;
const editorTextRegion = { x: 40, y: 96, width: 900, height: 40 };
const candidateRegion = { x: 40, y: 124, width: 600, height: 100 };
const commandTimeout = 5_000;
const stateTimeout = 15_000;

function capture(command, args, env = process.env, trimOutput = true) {
  const output = execFileSync(command, args, {
    cwd: workspace,
    encoding: "utf8",
    env,
    timeout: commandTimeout,
  });
  return trimOutput ? output.trim() : output;
}

async function waitUntil(check, description, timeout = stateTimeout) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const result = check();
    if (result) return result;
    await delay(50);
  }
  throw new Error(`timed out waiting for ${description}`);
}

function startProcess(children, name, command, args, env) {
  const child = spawn(command, args, {
    cwd: workspace,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const record = {
    name,
    child,
    stdout: "",
    stderr: "",
    spawnError: undefined,
    exited: undefined,
  };
  const append = (field, chunk) => {
    record[field] = (record[field] + chunk).slice(-1024 * 1024);
  };
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => append("stdout", chunk));
  child.stderr.on("data", (chunk) => append("stderr", chunk));
  record.exited = new Promise((resolveExit) => {
    child.once("error", (error) => {
      record.spawnError = error;
      resolveExit({ error });
    });
    child.once("close", (code, signal) => resolveExit({ code, signal }));
  });
  children.push(record);
  return record;
}

async function stopProcess(record) {
  if (record.spawnError) {
    await record.exited;
    return;
  }
  if (record.child.exitCode !== null || record.child.signalCode !== null) {
    await record.exited;
    return;
  }
  record.child.kill("SIGTERM");
  if (
    await Promise.race([
      record.exited.then(() => true),
      delay(2_000).then(() => false),
    ])
  ) {
    return;
  }
  record.child.kill("SIGKILL");
  if (
    !(await Promise.race([
      record.exited.then(() => true),
      delay(2_000).then(() => false),
    ]))
  ) {
    throw new Error(`failed to reap ${record.name} after SIGKILL`);
  }
}

function isolatedEnvironment(scratch) {
  const home = join(scratch, "home");
  const config = join(scratch, "config");
  const data = join(scratch, "data");
  const cache = join(scratch, "cache");
  const runtime = join(scratch, "runtime");
  for (
    const directory of [
      home,
      config,
      data,
      cache,
      runtime,
      join(config, "fcitx5"),
      join(config, "sway"),
    ]
  ) {
    mkdirSync(directory, { recursive: true });
  }
  chmodSync(runtime, 0o700);
  writeFileSync(
    join(config, "fcitx5", "profile"),
    `[Groups/0]\nName=Default\nDefault Layout=us\nDefaultIM=pinyin\n\n`
      + `[Groups/0/Items/0]\nName=keyboard-us\nLayout=\n\n`
      + `[Groups/0/Items/1]\nName=pinyin\nLayout=\n\n[GroupOrder]\n0=Default\n`,
  );
  writeFileSync(
    join(config, "sway", "config"),
    "output HEADLESS-1 resolution 1024x768 position 0,0 scale 1\n"
      + "default_border none\ndefault_floating_border none\n",
  );

  const environment = {
    ...process.env,
    HOME: home,
    XDG_CONFIG_HOME: config,
    XDG_DATA_HOME: data,
    XDG_CACHE_HOME: cache,
    XDG_RUNTIME_DIR: runtime,
    XDG_SESSION_TYPE: "wayland",
    XDG_CURRENT_DESKTOP: "sway",
    WLR_BACKENDS: "headless",
    WLR_HEADLESS_OUTPUTS: "1",
    WLR_RENDERER: "pixman",
    WLR_LIBINPUT_NO_DEVICES: "1",
    LIBGL_ALWAYS_SOFTWARE: "1",
    GALLIUM_DRIVER: "llvmpipe",
    VK_LOADER_LAYERS_DISABLE: "*",
    __GLX_VENDOR_LIBRARY_NAME: "mesa",
    NO_AT_BRIDGE: "1",
    RUST_BACKTRACE: "1",
    RUST_LOG: "info",
    LANG: "C.UTF-8",
    LC_ALL: "C.UTF-8",
  };
  delete environment.DISPLAY;
  delete environment.WAYLAND_DISPLAY;
  delete environment.XAUTHORITY;
  delete environment.XMODIFIERS;
  delete environment.GTK_IM_MODULE;
  delete environment.QT_IM_MODULE;
  delete environment.FONTCONFIG_PATH;
  if (process.env.METIS_NATIVE_FONTCONFIG_FILE) {
    assert.ok(
      existsSync(process.env.METIS_NATIVE_FONTCONFIG_FILE),
      "fixed fontconfig must exist",
    );
    environment.FONTCONFIG_FILE = process.env.METIS_NATIVE_FONTCONFIG_FILE;
  }

  const icdDirectories = [
    process.env.METIS_NATIVE_VULKAN_ICD_DIR,
    "/usr/share/vulkan/icd.d",
    "/run/opengl-driver/share/vulkan/icd.d",
  ].filter(Boolean);
  const lavapipe = icdDirectories.flatMap((directory) => {
    if (!existsSync(directory)) return [];
    return readdirSync(directory)
      .filter(
        (name) => name.startsWith("lvp_icd.") && name.endsWith(".json"),
      )
      .map((name) => join(directory, name));
  })[0];
  assert.ok(lavapipe, "Mesa lavapipe Vulkan ICD is required");
  environment.VK_DRIVER_FILES = lavapipe;
  const eglVendor = [
    process.env.METIS_NATIVE_EGL_VENDOR_FILE,
    "/usr/share/glvnd/egl_vendor.d/50_mesa.json",
    "/run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json",
  ]
    .filter(Boolean)
    .find((path) => existsSync(path));
  assert.ok(eglVendor, "Mesa EGL vendor configuration is required");
  environment.__EGL_VENDOR_LIBRARY_FILENAMES = eglVendor;
  return { environment, runtime };
}

function decodePpm(path) {
  const bytes = readFileSync(path);
  let offset = 0;
  const token = () => {
    while (offset < bytes.length) {
      if (bytes[offset] === 35) {
        while (offset < bytes.length && bytes[offset] !== 10) {
          offset += 1;
        }
      } else if (
        bytes[offset] === 32
        || bytes[offset] === 9
        || bytes[offset] === 10
        || bytes[offset] === 13
      ) {
        offset += 1;
      } else {
        break;
      }
    }
    const start = offset;
    while (
      offset < bytes.length
      && ![32, 9, 10, 13].includes(bytes[offset])
    ) {
      offset += 1;
    }
    return bytes.toString("ascii", start, offset);
  };
  assert.equal(token(), "P6", `${path}: only binary PPM is supported`);
  const width = Number(token());
  const height = Number(token());
  assert.ok(
    Number.isInteger(width)
      && width > 0
      && Number.isInteger(height)
      && height > 0,
  );
  assert.equal(
    Number(token()),
    255,
    `${path}: unsupported PPM channel range`,
  );
  assert.ok(
    bytes[offset] === 32
      || bytes[offset] === 9
      || bytes[offset] === 10
      || bytes[offset] === 13,
  );
  offset += 1;
  const data = Buffer.from(bytes.subarray(offset));
  assert.equal(
    data.length,
    width * height * 3,
    `${path}: truncated PPM pixels`,
  );
  return { width, height, data };
}

function crop(image, x, y, width, height) {
  assert.ok(
    x >= 0
      && y >= 0
      && x + width <= image.width
      && y + height <= image.height,
  );
  const data = Buffer.alloc(width * height * 3);
  for (let row = 0; row < height; row += 1) {
    const source = ((y + row) * image.width + x) * 3;
    image.data.copy(data, row * width * 3, source, source + width * 3);
  }
  return data;
}

function writeBmp(image, path) {
  const rowSize = Math.ceil((image.width * 3) / 4) * 4;
  const pixelBytes = rowSize * image.height;
  const output = Buffer.alloc(54 + pixelBytes);
  output.write("BM", 0);
  output.writeUInt32LE(output.length, 2);
  output.writeUInt32LE(54, 10);
  output.writeUInt32LE(40, 14);
  output.writeInt32LE(image.width, 18);
  output.writeInt32LE(image.height, 22);
  output.writeUInt16LE(1, 26);
  output.writeUInt16LE(24, 28);
  output.writeUInt32LE(pixelBytes, 34);
  for (let y = 0; y < image.height; y += 1) {
    for (let x = 0; x < image.width; x += 1) {
      const source = ((image.height - y - 1) * image.width + x) * 3;
      const target = 54 + y * rowSize + x * 3;
      output[target] = image.data[source + 2];
      output[target + 1] = image.data[source + 1];
      output[target + 2] = image.data[source];
    }
  }
  writeFileSync(path, output);
}

function captureImage(name, environment) {
  const ppm = join(artifacts, `${name}.ppm`);
  capture("grim", ["-t", "ppm", "-o", "HEADLESS-1", ppm], environment);
  const image = decodePpm(ppm);
  writeBmp(image, join(artifacts, `${name}.bmp`));
  return image;
}

function assertChineseFontProbes(image) {
  assert.equal(image.width, 1024, "headless Wayland output width");
  assert.equal(image.height, 768, "headless Wayland output height");
  const probes = [0, 1, 2, 3].map((index) => crop(image, 16 + index * 80, 16, 64, 64));
  for (const [index, probe] of probes.entries()) {
    const colors = new Map();
    for (let offset = 0; offset < probe.length; offset += 3) {
      const color = probe.readUIntBE(offset, 3);
      colors.set(color, (colors.get(color) ?? 0) + 1);
    }
    const backgroundCount = Math.max(...colors.values());
    assert.ok(
      probe.length / 3 - backgroundCount > 40,
      `Chinese font probe ${index} is blank or missing`,
    );
  }
  for (let left = 0; left < probes.length; left += 1) {
    for (let right = left + 1; right < probes.length; right += 1) {
      assert.ok(
        !probes[left].equals(probes[right]),
        `Chinese font probes ${left} and ${right} rendered identical glyphs`,
      );
    }
  }
}

function assertPixelChange(before, after, region, description) {
  assert.equal(before.width, after.width);
  assert.equal(before.height, after.height);
  let changed = 0;
  for (let y = region.y; y < region.y + region.height; y += 1) {
    for (let x = region.x; x < region.x + region.width; x += 1) {
      const offset = (y * before.width + x) * 3;
      const difference = [0, 1, 2].reduce(
        (sum, channel) =>
          sum
          + Math.abs(
            before.data[offset + channel]
              - after.data[offset + channel],
          ),
        0,
      );
      if (difference > 32) changed += 1;
    }
  }
  assert.ok(changed > 100, `${description}: only ${changed} pixels changed`);
}

async function waitForPixelChange(
  before,
  name,
  region,
  description,
  environment,
) {
  return waitUntil(() => {
    const after = captureImage(name, environment);
    try {
      assertPixelChange(before, after, region, description);
      return after;
    } catch {
      return false;
    }
  }, description);
}

function startHarness(children, name, executable, environment) {
  const record = startProcess(children, name, executable, [], environment);
  const harness = {
    record,
    snapshots: [],
    ready: undefined,
    protocolError: undefined,
  };
  let remainder = "";
  record.child.stdout.on("data", (chunk) => {
    remainder += chunk;
    const lines = remainder.split("\n");
    remainder = lines.pop();
    for (const line of lines) {
      if (!line) continue;
      try {
        const event = JSON.parse(line);
        if (event.type === "ready") harness.ready = event;
        if (event.type === "snapshot") harness.snapshots.push(event);
      } catch (error) {
        harness.protocolError = error;
      }
    }
  });
  return harness;
}

async function waitHarnessReady(harness) {
  await waitUntil(() => {
    if (harness.protocolError) throw harness.protocolError;
    if (harness.record.spawnError) throw harness.record.spawnError;
    if (
      harness.record.child.exitCode !== null
      || harness.record.child.signalCode !== null
    ) {
      throw new Error(
        `${harness.record.name} exited before ready: ${harness.record.stderr}`,
      );
    }
    return harness.ready;
  }, `${harness.record.name} readiness`);
  assert.equal(harness.ready.pid, harness.record.child.pid);
}

async function focusHarness(harness, environment) {
  await waitUntil(() => {
    try {
      capture(
        "swaymsg",
        [`[app_id="${harness.ready.app_id}"]`, "focus"],
        environment,
      );
      return true;
    } catch {
      return false;
    }
  }, `${harness.record.name} mapped and focused`);
  writeFileSync(
    join(artifacts, `${harness.record.name}-tree.json`),
    capture("swaymsg", ["-t", "get_tree"], environment),
  );
}

async function waitHarnessSnapshot(harness, predicate, description, start = 0) {
  let cursor = start;
  return waitUntil(() => {
    if (harness.protocolError) throw harness.protocolError;
    if (
      harness.record.child.exitCode !== null
      || harness.record.child.signalCode !== null
    ) {
      throw new Error(
        `${harness.record.name} exited while waiting for ${description}`,
      );
    }
    while (cursor < harness.snapshots.length) {
      const snapshot = harness.snapshots[cursor];
      cursor += 1;
      if (predicate(snapshot)) return snapshot;
    }
    return false;
  }, description);
}

async function main() {
  assert.equal(
    process.platform,
    "linux",
    "native Editor smoke requires Linux Wayland",
  );
  if (process.env.METIS_NATIVE_SMOKE_DBUS !== "1") {
    const result = spawnSync(
      "dbus-run-session",
      ["--", process.execPath, fileURLToPath(import.meta.url)],
      {
        cwd: workspace,
        env: {
          ...process.env,
          METIS_NATIVE_SMOKE_DBUS: "1",
          XDG_DATA_DIRS: "",
        },
        stdio: "inherit",
      },
    );
    if (result.error) throw result.error;
    process.exitCode = result.status ?? 1;
    return;
  }

  const scratch = mkdtempSync(join(tmpdir(), "metis-native-editor-"));
  const children = [];
  let success;
  try {
    rmSync(artifacts, { recursive: true, force: true });
    mkdirSync(artifacts, { recursive: true });
    execFileSync(
      "cargo",
      ["build", "-p", "metis", "--example", "native_editor_smoke"],
      {
        cwd: workspace,
        stdio: "inherit",
        timeout: 600_000,
      },
    );

    const { environment, runtime } = isolatedEnvironment(scratch);
    const sway = startProcess(
      children,
      "sway",
      "sway",
      [
        "--unsupported-gpu",
        "-c",
        join(scratch, "config", "sway", "config"),
      ],
      environment,
    );
    const waylandDisplay = await waitUntil(() => {
      if (sway.spawnError) throw sway.spawnError;
      if (sway.child.exitCode !== null) {
        throw new Error(
          `Sway exited before creating a Wayland socket: ${sway.stderr}`,
        );
      }
      return readdirSync(runtime).find((name) => /^wayland-\d+$/u.test(name));
    }, "the isolated Wayland socket");
    environment.WAYLAND_DISPLAY = waylandDisplay;
    const swayIpc = await waitUntil(
      () => readdirSync(runtime).find((name) => /^sway-ipc\..*\.sock$/u.test(name)),
      "the Sway IPC socket",
    );
    environment.SWAYSOCK = join(runtime, swayIpc);
    await waitUntil(() => {
      try {
        capture(
          "grim",
          [
            "-t",
            "ppm",
            "-o",
            "HEADLESS-1",
            join(scratch, "ready.ppm"),
          ],
          environment,
        );
        return true;
      } catch {
        return false;
      }
    }, "the headless Wayland output");

    // Keep the seat's keyboard capability alive between short wtype commands.
    const keyboard = startProcess(
      children,
      "keyboard",
      "wtype",
      ["-k", "Shift_L", "-s", "120000"],
      environment,
    );
    await waitUntil(() => {
      if (keyboard.spawnError) throw keyboard.spawnError;
      if (keyboard.child.exitCode !== null) {
        throw new Error(
          `persistent virtual keyboard exited: ${keyboard.stderr}`,
        );
      }
      const inputs = capture(
        "swaymsg",
        ["-t", "get_inputs"],
        environment,
      );
      writeFileSync(join(artifacts, "sway-inputs.json"), inputs);
      return inputs.includes("wlr_virtual_keyboard_v1");
    }, "a persistent virtual keyboard");

    const fcitx = startProcess(
      children,
      "fcitx5",
      "fcitx5",
      [
        "-D",
        "--verbose",
        "default=4,key_trace=5,classicui=5,waylandim=5",
      ],
      environment,
    );
    await waitUntil(() => {
      if (fcitx.spawnError) throw fcitx.spawnError;
      if (fcitx.child.exitCode !== null) {
        throw new Error(
          `isolated Fcitx5 exited before acquiring its D-Bus name: ${fcitx.stderr}`,
        );
      }
      const owned = capture(
        "dbus-send",
        [
          "--session",
          "--dest=org.freedesktop.DBus",
          "--type=method_call",
          "--print-reply",
          "/org/freedesktop/DBus",
          "org.freedesktop.DBus.NameHasOwner",
          "string:org.fcitx.Fcitx5",
        ],
        environment,
      );
      return owned.includes("boolean true");
    }, "the isolated Fcitx5 service");
    await waitUntil(() => {
      try {
        return (
          capture(
            "fcitx5-remote",
            ["--check", "-m", "pinyin"],
            environment,
          ) === "pinyin"
        );
      } catch {
        return false;
      }
    }, "the pinyin engine");

    const target = process.env.CARGO_TARGET_DIR
      ? resolve(workspace, process.env.CARGO_TARGET_DIR)
      : join(workspace, "target");
    const executable = join(
      target,
      "debug",
      "examples",
      "native_editor_smoke",
    );
    const editor = startHarness(
      children,
      "editor",
      executable,
      environment,
    );
    const snapshots = editor.snapshots;
    await waitHarnessReady(editor);
    await focusHarness(editor, environment);
    const waitSnapshot = (predicate, description, start = 0) =>
      waitHarnessSnapshot(editor, predicate, description, start);

    capture("wtype", ["x"], environment);
    await waitSnapshot(
      (snapshot) => snapshot.value === "x",
      "native virtual-keyboard input",
    );
    capture("wtype", ["-k", "BackSpace"], environment);
    await waitSnapshot(
      (snapshot) => snapshot.value === "",
      "Backspace to restore the empty Editor",
    );
    await delay(500);
    const initialImage = captureImage("initial", environment);
    assertChineseFontProbes(initialImage);
    capture("fcitx5-remote", ["--check", "-s", "pinyin"], environment);
    capture("fcitx5-remote", ["--check", "-o"], environment);

    let start = snapshots.length;
    capture("wtype", ["-d", "100", "nihao"], environment);
    const composing = await waitSnapshot(
      (snapshot) =>
        snapshot.marked
        && typeof snapshot.preedit === "string"
        && snapshot.preedit,
      "visible pinyin composition",
      start,
    );
    assert.ok(composing.preedit.length > 0);
    const composingImage = await waitForPixelChange(
      initialImage,
      "composing",
      candidateRegion,
      "Fcitx5 candidate window must be visible",
      environment,
    );

    start = snapshots.length;
    capture("wtype", ["-k", "space"], environment);
    await waitSnapshot(
      (snapshot) => snapshot.value === "你好" && !snapshot.marked,
      "the first pinyin candidate to commit",
      start,
    );
    await waitForPixelChange(
      composingImage,
      "committed",
      candidateRegion,
      "candidate commit must update the native frame",
      environment,
    );
    const committedImage = await waitForPixelChange(
      initialImage,
      "committed",
      editorTextRegion,
      "committed text must be visible in the native frame",
      environment,
    );

    const clipboardSource = startProcess(
      children,
      "wl-copy",
      "wl-copy",
      ["--foreground", clipboardText],
      environment,
    );
    await waitUntil(() => {
      if (clipboardSource.spawnError) throw clipboardSource.spawnError;
      if (
        clipboardSource.child.exitCode !== null
        || clipboardSource.child.signalCode !== null
      ) {
        throw new Error(
          `wl-copy exited before providing the paste source: ${clipboardSource.stderr}`,
        );
      }
      try {
        return (
          capture(
            "wl-paste",
            ["--no-newline"],
            environment,
            false,
          ) === clipboardText
        );
      } catch {
        return false;
      }
    }, "the external clipboard client to own the paste source");

    start = snapshots.length;
    capture("wtype", ["-M", "ctrl", "-k", "v", "-m", "ctrl"], environment);
    await waitSnapshot(
      (snapshot) => snapshot.value === expectedText && !snapshot.marked,
      "native clipboard paste",
      start,
    );
    await waitForPixelChange(
      committedImage,
      "pasted",
      editorTextRegion,
      "pasted text must be visible in the native frame",
      environment,
    );

    start = snapshots.length;
    capture("wtype", ["-M", "ctrl", "-k", "a", "-m", "ctrl"], environment);
    await waitSnapshot(
      (snapshot) => snapshot.selection_start === 0 && snapshot.selection_end > 0,
      "Editor select-all",
      start,
    );
    capture("wtype", ["-M", "ctrl", "-k", "c", "-m", "ctrl"], environment);
    const externalCopy = await waitUntil(() => {
      try {
        const value = capture(
          "wl-paste",
          ["--no-newline"],
          environment,
          false,
        );
        return value === expectedText ? value : false;
      } catch {
        return false;
      }
    }, "the external clipboard client to read the copied text");

    await delay(200);
    assertChineseFontProbes(captureImage("final", environment));
    assert.match(
      editor.record.stderr,
      /Selected GPU adapter:.*llvmpipe.*\(Vulkan\)/u,
    );
    writeFileSync(
      join(artifacts, "result.json"),
      `${
        JSON.stringify(
          {
            platform,
            value: expectedText,
            clipboard: externalCopy,
            inputMethod: "Fcitx5 pinyin (external Wayland input-method service)",
            compositor: capture("sway", ["--version"], environment),
            renderer: "Mesa lavapipe Vulkan",
          },
          null,
          2,
        )
      }\n`,
    );
    success = {
      value: expectedText,
      lavapipe: environment.VK_DRIVER_FILES,
    };
  } finally {
    const cleanupErrors = [];
    for (const record of [...children].reverse()) {
      try {
        await stopProcess(record);
      } catch (error) {
        cleanupErrors.push(error);
      }
    }
    for (const record of children) {
      writeFileSync(
        join(artifacts, `${record.name}-stdout.log`),
        record.stdout,
      );
      writeFileSync(
        join(artifacts, `${record.name}-stderr.log`),
        record.stderr,
      );
    }
    rmSync(scratch, { recursive: true, force: true });
    if (cleanupErrors.length > 0) {
      throw new AggregateError(
        cleanupErrors,
        "native smoke cleanup failed",
      );
    }
  }
  if (success) {
    console.log(`native Editor smoke passed: ${JSON.stringify(success)}`);
    console.log(`native Editor artifacts: ${artifacts}`);
  }
}

try {
  await main();
} catch (error) {
  console.error(`native Editor artifacts: ${artifacts}`);
  throw error;
}
