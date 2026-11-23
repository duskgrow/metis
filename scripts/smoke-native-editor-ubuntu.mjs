import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const workspace = fileURLToPath(new URL("..", import.meta.url));
const nodeVersion = readFileSync(
  new URL("../.node-version", import.meta.url),
  "utf8",
).trim();
const toolchainFile = readFileSync(
  new URL("../rust-toolchain.toml", import.meta.url),
  "utf8",
);
const rustToolchain = toolchainFile.match(
  /^channel\s*=\s*"([^"]+)"\s*$/mu,
)?.[1];
if (!rustToolchain) throw new Error("rust-toolchain.toml has no channel");

const nixSystem = {
  x64: "x86_64-linux",
  arm64: "aarch64-linux",
}[process.arch];
if (!nixSystem) {
  throw new Error(`unsupported native smoke architecture: ${process.arch}`);
}
const nodeNix = readFileSync(
  new URL("../nix/node.nix", import.meta.url),
  "utf8",
);
const systemBlock = nodeNix.match(
  new RegExp(`${nixSystem} = \\{([\\s\\S]*?)\\}`, "u"),
)?.[1];
const nodeHash = systemBlock?.match(/hash = "sha256-([^"]+)"/u)?.[1];
if (!nodeHash) {
  throw new Error(`nix/node.nix has no Node.js hash for ${nixSystem}`);
}
const nodeSha256 = Buffer.from(nodeHash, "base64").toString("hex");
if (!/^[0-9a-f]{64}$/u.test(nodeSha256)) {
  throw new Error("invalid pinned Node.js SHA-256");
}

const rustupVersion = "1.28.2";
const rustupSha256 = {
  x64: "20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c",
  arm64: "e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c",
}[process.arch];
const image = "metis-native-editor-smoke:ubuntu-24.04";
const imageLabel = "dev.metis.native-editor-smoke=ubuntu-24.04";

const dockerfile = `
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac
ARG NODE_VERSION
ARG NODE_SHA256
ARG RUST_TOOLCHAIN
ARG RUSTUP_VERSION
ARG RUSTUP_SHA256
ARG APT_SNAPSHOT=20261123T000000Z
ARG WAYLAND_VERSION=1.23.1
ARG WAYLAND_SHA256=158ec49af498f2558c7fbf7e8b070d010d4e270cc6076003a18a6c813f87e244
ARG WLROOTS_VERSION=0.18.3
ARG WLROOTS_SHA256=e9af941648cdc72b2ea39ceb6f1d54905f9038d968415a197df7d168923e415c
ARG LIBINPUT_VERSION=1.26.2
ARG LIBINPUT_SHA256=5c1c4150f217fea1db2d1fd88e2607b2f1928cfde65c34da65a9f24dcfd69464
ARG SWAY_VERSION=1.10.1
ARG SWAY_SHA256=8565ab3b359780f02b1dcb24dc48e5b6b82c64dd97ca795782c2fb4cab62457b
LABEL ${imageLabel}
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get -o Acquire::Retries=3 update \
    && apt-get -o Acquire::Retries=3 install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN rm -f /etc/apt/sources.list.d/ubuntu.sources \
    && printf '%s\\n' \
    "deb https://snapshot.ubuntu.com/ubuntu/\${APT_SNAPSHOT}/ noble main restricted universe multiverse" \
    "deb https://snapshot.ubuntu.com/ubuntu/\${APT_SNAPSHOT}/ noble-updates main restricted universe multiverse" \
    "deb https://snapshot.ubuntu.com/ubuntu/\${APT_SNAPSHOT}/ noble-security main restricted universe multiverse" \
    "deb https://snapshot.ubuntu.com/ubuntu/\${APT_SNAPSHOT}/ noble-backports main restricted universe multiverse" \
    > /etc/apt/sources.list
RUN apt-get -o Acquire::Retries=3 update \
    && apt-get -o Acquire::Retries=3 install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    clang \
    cmake \
    curl \
    dbus \
    fcitx5 \
    fcitx5-chinese-addons \
    fonts-noto-cjk \
    git \
    grim \
    libcairo2-dev \
    libdrm-dev \
    libevdev-dev \
    libffi-dev \
    libfontconfig1-dev \
    libglvnd-dev \
    libjson-c-dev \
    libmtdev-dev \
    libpango1.0-dev \
    libpcre2-dev \
    libpixman-1-dev \
    libssl-dev \
    libudev-dev \
    libvulkan-dev \
    libwayland-dev \
    libx11-dev \
    libxcb1-dev \
    libxkbcommon-dev \
    libxkbcommon-x11-dev \
    libzstd-dev \
    mesa-vulkan-drivers \
    meson \
    ninja-build \
    pkg-config \
    wayland-protocols \
    wl-clipboard \
    wtype \
    xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    cd /tmp; \
    curl --proto '=https' --tlsv1.2 -fsSL \
    "https://gitlab.freedesktop.org/wayland/wayland/-/archive/\${WAYLAND_VERSION}/wayland-\${WAYLAND_VERSION}.tar.gz" \
    -o wayland.tar.gz; \
    echo "\${WAYLAND_SHA256}  wayland.tar.gz" | sha256sum -c -; \
    tar -xzf wayland.tar.gz; \
    meson setup "wayland-\${WAYLAND_VERSION}/build" "wayland-\${WAYLAND_VERSION}" \
    --prefix=/usr/local --libdir=lib --buildtype=release \
    -Ddocumentation=false -Dtests=false -Ddtd_validation=false; \
    ninja -C "wayland-\${WAYLAND_VERSION}/build"; \
    meson install -C "wayland-\${WAYLAND_VERSION}/build"; \
    ldconfig; \
    rm -rf wayland.tar.gz "wayland-\${WAYLAND_VERSION}"
RUN set -eu; \
    cd /tmp; \
    curl --proto '=https' --tlsv1.2 -fsSL \
    "https://gitlab.freedesktop.org/wlroots/wlroots/-/archive/\${WLROOTS_VERSION}/wlroots-\${WLROOTS_VERSION}.tar.gz" \
    -o wlroots.tar.gz; \
    echo "\${WLROOTS_SHA256}  wlroots.tar.gz" | sha256sum -c -; \
    tar -xzf wlroots.tar.gz; \
    meson setup "wlroots-\${WLROOTS_VERSION}/build" "wlroots-\${WLROOTS_VERSION}" \
    --prefix=/usr/local --libdir=lib --buildtype=release \
    -Dbackends=[] -Drenderers=[] -Dallocators=[] \
    -Dsession=disabled -Dxwayland=disabled -Dexamples=false \
    -Dcolor-management=disabled -Dlibliftoff=disabled -Dxcb-errors=disabled; \
    ninja -C "wlroots-\${WLROOTS_VERSION}/build"; \
    meson install -C "wlroots-\${WLROOTS_VERSION}/build"; \
    ldconfig; \
    rm -rf wlroots.tar.gz "wlroots-\${WLROOTS_VERSION}"
RUN set -eu; \
    cd /tmp; \
    curl --proto '=https' --tlsv1.2 -fsSL \
    "https://gitlab.freedesktop.org/libinput/libinput/-/archive/\${LIBINPUT_VERSION}/libinput-\${LIBINPUT_VERSION}.tar.gz" \
    -o libinput.tar.gz; \
    echo "\${LIBINPUT_SHA256}  libinput.tar.gz" | sha256sum -c -; \
    tar -xzf libinput.tar.gz; \
    meson setup "libinput-\${LIBINPUT_VERSION}/build" "libinput-\${LIBINPUT_VERSION}" \
    --prefix=/usr/local --libdir=lib --buildtype=release \
    -Dlibwacom=false -Ddebug-gui=false -Dtests=false \
    -Ddocumentation=false -Dzshcompletiondir=no; \
    ninja -C "libinput-\${LIBINPUT_VERSION}/build"; \
    meson install -C "libinput-\${LIBINPUT_VERSION}/build"; \
    ldconfig; \
    rm -rf libinput.tar.gz "libinput-\${LIBINPUT_VERSION}"
RUN set -eu; \
    cd /tmp; \
    curl --proto '=https' --tlsv1.2 -fsSL \
    "https://github.com/swaywm/sway/archive/refs/tags/\${SWAY_VERSION}.tar.gz" \
    -o sway.tar.gz; \
    echo "\${SWAY_SHA256}  sway.tar.gz" | sha256sum -c -; \
    tar -xzf sway.tar.gz; \
    export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig; \
    meson setup "sway-\${SWAY_VERSION}/build" "sway-\${SWAY_VERSION}" \
    --prefix=/usr/local --libdir=lib --buildtype=release \
    -Ddefault-wallpaper=false -Dzsh-completions=false \
    -Dbash-completions=false -Dfish-completions=false \
    -Dswaybar=false -Dswaynag=false -Dtray=disabled \
    -Dgdk-pixbuf=disabled -Dman-pages=disabled; \
    ninja -C "sway-\${SWAY_VERSION}/build"; \
    meson install -C "sway-\${SWAY_VERSION}/build"; \
    ldconfig; \
    test "$(sway --version)" = "sway version \${SWAY_VERSION}"; \
    rm -rf sway.tar.gz "sway-\${SWAY_VERSION}"
RUN set -eu; \
    case "$(uname -m)" in x86_64) node_arch=x64 ;; aarch64) node_arch=arm64 ;; *) exit 1 ;; esac; \
    cd /tmp; \
    archive="node-v\${NODE_VERSION}-linux-\${node_arch}.tar.xz"; \
    curl --proto '=https' --tlsv1.2 -fsSLO "https://nodejs.org/dist/v\${NODE_VERSION}/\${archive}"; \
    echo "\${NODE_SHA256}  \${archive}" | sha256sum -c -; \
    mkdir -p /opt/node; \
    tar -xJf "\${archive}" -C /opt/node --strip-components=1; \
    rm -f "\${archive}"
USER ubuntu
RUN set -eu; \
    case "$(uname -m)" in x86_64) rust_target=x86_64-unknown-linux-gnu ;; aarch64) rust_target=aarch64-unknown-linux-gnu ;; *) exit 1 ;; esac; \
    curl --proto '=https' --tlsv1.2 -fsSL \
    "https://static.rust-lang.org/rustup/archive/\${RUSTUP_VERSION}/\${rust_target}/rustup-init" \
    -o /tmp/rustup-init; \
    echo "\${RUSTUP_SHA256}  /tmp/rustup-init" | sha256sum -c -; \
    chmod +x /tmp/rustup-init; \
    /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "\${RUST_TOOLCHAIN}"; \
    rm -f /tmp/rustup-init
ENV HOME=/home/ubuntu \
    PATH=/home/ubuntu/.cargo/bin:/opt/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    RUSTUP_TOOLCHAIN=\${RUST_TOOLCHAIN}
WORKDIR /src
`;

const context = mkdtempSync(join(tmpdir(), "metis-native-editor-docker-"));
try {
  execFileSync(
    "docker",
    [
      "build",
      "--build-arg",
      `NODE_VERSION=${nodeVersion}`,
      "--build-arg",
      `NODE_SHA256=${nodeSha256}`,
      "--build-arg",
      `RUST_TOOLCHAIN=${rustToolchain}`,
      "--build-arg",
      `RUSTUP_VERSION=${rustupVersion}`,
      "--build-arg",
      `RUSTUP_SHA256=${rustupSha256}`,
      "--tag",
      image,
      "--file",
      "-",
      context,
    ],
    {
      cwd: workspace,
      input: dockerfile,
      stdio: ["pipe", "inherit", "inherit"],
      timeout: 1_200_000,
    },
  );
} finally {
  rmSync(context, { recursive: true, force: true });
}
execFileSync(
  "docker",
  ["image", "prune", "--force", "--filter", `label=${imageLabel}`],
  { cwd: workspace, stdio: "inherit", timeout: 30_000 },
);

const container = `metis-native-editor-smoke-${process.pid}`;
let completed = false;
try {
  execFileSync(
    "docker",
    [
      "run",
      "--rm",
      "--name",
      container,
      "--env",
      "CARGO_HOME=/src/target/native-editor-ubuntu-cargo-home",
      "--env",
      "CARGO_TARGET_DIR=/src/target/native-editor-ubuntu-target",
      "--env",
      "METIS_NATIVE_PLATFORM=ubuntu-24.04",
      "--volume",
      `${workspace}:/src`,
      image,
      "node",
      "scripts/smoke-native-editor.mjs",
    ],
    { cwd: workspace, stdio: "inherit", timeout: 300_000 },
  );
  completed = true;
} finally {
  if (!completed) {
    spawnSync("docker", ["rm", "--force", container], {
      cwd: workspace,
      stdio: "ignore",
      timeout: 30_000,
    });
  }
}
