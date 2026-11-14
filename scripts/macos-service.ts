#!/usr/bin/env bun
/**
 * Run the voxtype daemon as a macOS LaunchAgent: starts at login and is
 * restarted by launchd whenever it exits or crashes. Fork addition
 * (zoos1/voxtype).
 *
 *   bun scripts/macos-service.ts install    # bundle + LaunchAgent, start now
 *   bun scripts/macos-service.ts refresh    # after a rebuild: update + restart
 *   bun scripts/macos-service.ts status
 *   bun scripts/macos-service.ts uninstall
 *
 * The daemon runs from /Applications/Voxtype.app so macOS grants
 * Microphone, Accessibility and Input Monitoring to "Voxtype" itself rather
 * than to whichever terminal started it. Only the main `voxtype` binary goes
 * in the bundle; the overlay binaries stay on PATH, so rebuilding the
 * overlay never resets those permissions. Replacing the main binary does
 * (the bundle is ad-hoc signed), so `refresh` copies it only when it changed.
 */
import { $ } from "bun";
import { existsSync, mkdirSync, copyFileSync, renameSync, chmodSync, rmSync } from "fs";
import { homedir } from "os";
import { join } from "path";

const LABEL = "io.voxtype.daemon"; // same id as upstream's app bundle
const APP = "/Applications/Voxtype.app";
const EXE = join(APP, "Contents/MacOS/voxtype-bin");
const PLIST = join(homedir(), "Library/LaunchAgents", `${LABEL}.plist`);
const LOGS = join(homedir(), "Library/Logs/voxtype");
const UID = process.getuid!();
const DOMAIN = `gui/${UID}`;

if (process.platform !== "darwin") throw new Error("macOS only");

async function sourceBinary(): Promise<string> {
  const p = Bun.which("voxtype");
  if (!p) throw new Error("voxtype not on PATH; run scripts/macos-local-install.ts first");
  return (await $`realpath ${p}`.text()).trim();
}

async function sha(path: string): Promise<string> {
  return existsSync(path) ? (await $`shasum -a 256 ${path}`.text()).split(" ")[0] : "";
}

function infoPlist(version: string): string {
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>voxtype-bin</string>
    <key>CFBundleIdentifier</key><string>${LABEL}</string>
    <key>CFBundleName</key><string>Voxtype</string>
    <key>CFBundleDisplayName</key><string>Voxtype</string>
    <key>CFBundleVersion</key><string>${version}</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSMicrophoneUsageDescription</key><string>Voxtype needs microphone access for speech-to-text transcription.</string>
    <key>NSAppleEventsUsageDescription</key><string>Voxtype needs accessibility access to type transcribed text.</string>
    <key>NSInputMonitoringUsageDescription</key><string>Voxtype monitors keyboard input to detect your push-to-talk hotkey.</string>
</dict>
</plist>
`;
}

function agentPlist(): string {
  const brewBin = existsSync("/opt/homebrew/bin") ? "/opt/homebrew/bin" : "/usr/local/bin";
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${EXE}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>5</integer>
    <key>LimitLoadToSessionType</key><string>Aqua</string>
    <key>ProcessType</key><string>Interactive</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key><string>${brewBin}:/usr/local/bin:/usr/bin:/bin</string>
    </dict>
    <key>StandardOutPath</key><string>${join(LOGS, "daemon.out.log")}</string>
    <key>StandardErrorPath</key><string>${join(LOGS, "daemon.err.log")}</string>
</dict>
</plist>
`;
}

/** Create or update the bundle. Returns true when the binary changed. */
async function writeBundle(): Promise<boolean> {
  const src = await sourceBinary();
  const version = (await $`${src} --version`.text()).trim().split(/\s+/).pop() ?? "0";
  mkdirSync(join(APP, "Contents/MacOS"), { recursive: true });
  await Bun.write(join(APP, "Contents/Info.plist"), infoPlist(version));
  const changed = (await sha(src)) !== (await sha(EXE));
  if (changed) {
    const staged = `${EXE}.new`;
    copyFileSync(src, staged);
    chmodSync(staged, 0o755);
    renameSync(staged, EXE);
  }
  await $`codesign --force --sign - --identifier ${LABEL} ${EXE}`.quiet();
  await $`codesign --force --sign - ${APP}`.quiet();
  if (changed) {
    // The ad-hoc signature changed, so old grants no longer match: clear
    // them so macOS asks again instead of silently refusing.
    for (const svc of ["Microphone", "Accessibility", "ListenEvent"]) {
      await $`tccutil reset ${svc} ${LABEL}`.quiet().nothrow();
    }
  }
  return changed;
}

async function loaded(): Promise<boolean> {
  return (await $`launchctl print ${DOMAIN}/${LABEL}`.quiet().nothrow()).exitCode === 0;
}

async function start() {
  if (await loaded()) await $`launchctl bootout ${DOMAIN}/${LABEL}`.quiet().nothrow();
  await $`launchctl bootstrap ${DOMAIN} ${PLIST}`;
  await $`launchctl enable ${DOMAIN}/${LABEL}`;
}

async function status() {
  const r = await $`launchctl print ${DOMAIN}/${LABEL}`.quiet().nothrow();
  if (r.exitCode !== 0) return console.log("not loaded");
  const out = r.stdout.toString();
  const pick = (k: string) => out.match(new RegExp(`^\\s*${k} = (.*)$`, "m"))?.[1] ?? "-";
  console.log(`state: ${pick("state")}  pid: ${pick("pid")}  runs: ${pick("runs")}  last exit: ${pick("last exit code")}`);
}

const PERMS = `Grant "Voxtype" in System Settings > Privacy & Security:
  Microphone (allow the prompt), Accessibility, Input Monitoring.
Then: bun scripts/macos-service.ts refresh`;

switch (process.argv[2]) {
  case "install": {
    mkdirSync(LOGS, { recursive: true });
    const changed = await writeBundle();
    await Bun.write(PLIST, agentPlist());
    // A daemon started from a terminal holds the lock; stop it first.
    await $`pkill -f "^voxtype daemon"`.quiet().nothrow();
    await start();
    await status();
    if (changed) console.log(`\n${PERMS}`);
    break;
  }
  case "refresh": {
    const changed = await writeBundle();
    await $`launchctl kickstart -k ${DOMAIN}/${LABEL}`;
    await status();
    if (changed) console.log(`\nBinary changed. ${PERMS}`);
    break;
  }
  case "status":
    await status();
    break;
  case "uninstall":
    if (await loaded()) await $`launchctl bootout ${DOMAIN}/${LABEL}`.nothrow();
    rmSync(PLIST, { force: true });
    console.log(`LaunchAgent removed. ${APP} left in place (delete it by hand if unwanted).`);
    break;
  default:
    console.log("usage: bun scripts/macos-service.ts install|refresh|status|uninstall");
    process.exit(2);
}
