# Subspace

Subspace is a library and CLI tool for managing a tree of Git worktrees.

## Install

### CLI

Install with cargo

```sh
cargo install --locked --path bin
```

### Pi tool

Build the Pi tool and its TypeScript binding:

```sh
npm ci
npm run build
```

Load the package in Pi:

```sh
pi install /absolute/path/to/subspace
```
