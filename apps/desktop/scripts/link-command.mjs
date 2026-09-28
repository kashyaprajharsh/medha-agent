import { existsSync, lstatSync, mkdirSync, readlinkSync, symlinkSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

if (process.platform !== "darwin") {
  throw new Error("This command linker currently supports macOS builds only.");
}

const desktop = resolve(fileURLToPath(new URL("..", import.meta.url)));
const app = join(desktop, "src-tauri", "target", "release", "bundle", "macos", "Medha.app");
const binary = join(app, "Contents", "MacOS", "medha-desktop");
if (!existsSync(binary)) {
  throw new Error("Build the desktop app first: npm run tauri build -- --bundles app");
}

const directory = join(homedir(), ".local", "bin");
const command = join(directory, "medha-desktop");
mkdirSync(directory, { recursive: true });
if (existsSync(command) || lstatIfPresent(command)) {
  if (!lstatSync(command).isSymbolicLink() || readlinkSync(command) !== binary) {
    throw new Error(`${command} already exists; refusing to replace it`);
  }
} else {
  symlinkSync(binary, command);
}
console.log(`Linked ${command} → ${binary}`);
console.log("Run medha-desktop [workspace] (ensure ~/.local/bin is on PATH).");

function lstatIfPresent(path) {
  try { return lstatSync(path); } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}
