#!/usr/bin/env bun
/**
 * macOS local build + install for this fork (zoos1/voxtype).
 *
 * Builds voxtype with Parakeet (ONNX) + Metal, installs the daemon, the
 * `voxtype-osd` launcher and the native `voxtype-osd-macos` overlay into
 * ~/.local/opt/voxtype-<version>/, symlinks them into the Homebrew bin dir,
 * downloads the Parakeet int8 model, and selects it in the config.
 *
 *   bun scripts/macos-local-install.ts            # build + install + model + config
 *   bun scripts/macos-local-install.ts --no-build # reinstall an existing build
 *   bun scripts/macos-local-install.ts --no-config
 *
 * Idempotent: safe to re-run after every pull or local commit.
 */
import { $ } from "bun";
import { existsSync, mkdirSync, renameSync, copyFileSync, chmodSync } from "fs";
import { homedir } from "os";
import { join } from "path";

const args = new Set(process.argv.slice(2));
const repo = join(import.meta.dir, "..");
const MODEL = "parakeet-tdt-0.6b-v3-int8";
const FEATURES = "gpu-metal,parakeet";
const BINS = ["voxtype", "voxtype-osd", "voxtype-osd-macos"];

if (process.platform !== "darwin") throw new Error("macOS only");

for (const tool of ["cargo", "cmake"]) {
  if (!Bun.which(tool)) {
    throw new Error(`missing '${tool}': brew install ${tool === "cargo" ? "rust" : "cmake"}`);
  }
}

const cargoToml = await Bun.file(join(repo, "Cargo.toml")).text();
const version = cargoToml.match(/^version = "([^"]+)"/m)?.[1];
if (!version) throw new Error("could not read version from Cargo.toml");

if (!args.has("--no-build")) {
  console.log(`==> cargo build --release --features ${FEATURES}`);
  // cmake 4 refuses CMakeLists with cmake_minimum_required < 3.5 (whisper.cpp deps).
  await $`cargo build --release --features ${FEATURES}`
    .cwd(repo)
    .env({ ...process.env, CMAKE_POLICY_VERSION_MINIMUM: "3.5" });
}

const dest = join(homedir(), ".local", "opt", `voxtype-${version}`);
mkdirSync(dest, { recursive: true });
const brewBin = Bun.which("brew")
  ? (await $`brew --prefix`.text()).trim() + "/bin"
  : "/usr/local/bin";

for (const bin of BINS) {
  const src = join(repo, "target", "release", bin);
  if (!existsSync(src)) throw new Error(`not built: ${src}`);
  // Copy then rename: replaces a running binary with a new inode instead of
  // rewriting it in place (which macOS code signing punishes with SIGKILL).
  const staged = join(dest, `.${bin}.new`);
  copyFileSync(src, staged);
  chmodSync(staged, 0o755);
  renameSync(staged, join(dest, bin));
  await $`ln -sfn ${join(dest, bin)} ${join(brewBin, bin)}`;
  console.log(`installed ${bin} -> ${join(brewBin, bin)}`);
}

const voxtype = join(dest, "voxtype");
console.log(`==> model ${MODEL}`);
await $`${voxtype} setup --download --model ${MODEL} --no-post-install --quiet`;

if (!args.has("--no-config")) {
  await $`${voxtype} config set engine parakeet`;
  await $`${voxtype} config set parakeet.model ${MODEL}`;
  await $`${voxtype} config set osd.enabled true`;
}

const installed = (await $`${voxtype} --version`.text()).trim();
console.log(`\n${installed} installed. Restart the daemon (Ctrl-C, then: voxtype daemon).`);
