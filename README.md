# metis

[中文正本](./README.zh-CN.md) — English translation for reference only.

[![CI](https://github.com/duskgrow/metis/actions/workflows/ci.yml/badge.svg)](https://github.com/duskgrow/metis/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](./LICENSE-MIT)

A personal multi-host agent workbench for continuing sessions, jobs, and approvals across devices, with integrated editing and diff review.

## Install

Git must be installed on each host running Metis.

After the first release, use the installer from GitHub Releases:

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/duskgrow/metis/releases/latest/download/metis-installer.sh | sh

crates.io installation becomes available after opting into registry publishing
and publishing the crate; see the Releasing section in CONTRIBUTING.md.

## Usage

Run `metis` to open the workbench. Use `metis --help` for command help. In a headless package, run `bin/metis host --home <state-directory>` to start a persistent Host.

## Design

- [Product architecture and runtime model](./docs/architecture.md)
- [Product scope and milestones](./docs/product-scope.md)
- [DeepSeek Harness integration research](./docs/research/deepseek-harness.md)

## Development

    direnv allow   # or: nix develop (entering the shell also arms the git hooks)
    just ci        # core local quality gate; CI adds platform and hosting checks

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## License

MIT OR Apache-2.0 — see [LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE).
