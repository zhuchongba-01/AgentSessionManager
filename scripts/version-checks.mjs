import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { synchronizeVersion } from "./version.mjs";

// Synthetic fixtures only; tests never modify the project's release metadata.
const files = {
  "package.json":
    '{"name":"agent-session-manager","version":"1.3.16","dependencies":{"example":"1.3.16"}}\n',
  "src-tauri/Cargo.toml":
    '[package]\nname = "agent-session-manager"\nversion = "1.3.16"\n\n[dependencies]\nexample = "1.3.16"\n',
  "src-tauri/Cargo.lock":
    'version = 4\r\n\r\n[[package]]\r\nname = "agent-session-manager"\r\nversion = "1.3.16"\r\n\r\n[[package]]\r\nname = "other"\r\nversion = "1.3.16"\r\n',
  "src-tauri/tauri.conf.json":
    '{"version":"1.3.16","app":{"windows":[{"title":"Agent会话管理器  v1.3.16"}]}}\n',
  "src-tauri/tauri.windows.conf.json":
    '{"app":{"windows":[{"title":"Agent会话管理器  v1.3.16"}]}}\n',
  "installer/AgentSessionManager.iss":
    '#define MyAppVersion "1.3.16"\r\n[Setup]\r\nAppVersion={#MyAppVersion}\r\n',
};

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "asm-version-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  for (const [name, text] of Object.entries(files)) {
    mkdirSync(dirname(join(root, name)), { recursive: true });
    writeFileSync(join(root, name), text);
  }
  return root;
}

function snapshot(root) {
  return Object.fromEntries(
    Object.keys(files).map((name) => [
      name,
      readFileSync(join(root, name), "utf8"),
    ]),
  );
}

test("check is read-only and accepts aligned metadata", (t) => {
  const root = fixture(t);
  const before = snapshot(root);
  assert.deepEqual(synchronizeVersion(root, { check: true }), {
    version: "1.3.16",
    changed: [],
  });
  assert.deepEqual(snapshot(root), before);
});

test("updates all release metadata without touching dependencies or line endings", (t) => {
  const root = fixture(t);
  const result = synchronizeVersion(root, { version: "1.3.17" });
  assert.equal(result.changed.length, 6);
  const after = snapshot(root);
  for (const [name, text] of Object.entries(files)) {
    const expected =
      name === "src-tauri/Cargo.toml" ||
      name === "src-tauri/Cargo.lock" ||
      name === "package.json"
        ? text.replace("1.3.16", "1.3.17")
        : text.replaceAll("1.3.16", "1.3.17");
    assert.equal(after[name], expected, name);
  }
  assert.equal(synchronizeVersion(root, { check: true }).version, "1.3.17");
  assert.deepEqual(synchronizeVersion(root, { version: "1.3.17" }).changed, []);
});

test("check reports drift without repairing or writing files", (t) => {
  const root = fixture(t);
  writeFileSync(
    join(root, "installer/AgentSessionManager.iss"),
    files["installer/AgentSessionManager.iss"].replace("1.3.16", "1.0.0"),
  );
  const before = snapshot(root);
  assert.throws(
    () => synchronizeVersion(root, { check: true }),
    /Version mismatch.*installer/,
  );
  assert.deepEqual(snapshot(root), before);
});

test("rejects unsupported version strings without mutations", (t) => {
  const root = fixture(t);
  const before = snapshot(root);
  for (const version of [
    undefined,
    "",
    "v1.3.17",
    "1.3",
    "01.3.17",
    "1.3.17-beta.1",
    "1.3.17\n",
    "../oops",
  ]) {
    assert.throws(
      () => synchronizeVersion(root, { version }),
      /Expected a stable version/,
    );
  }
  assert.deepEqual(snapshot(root), before);
});

test("validates late-file metadata before writing any earlier file", (t) => {
  const root = fixture(t);
  writeFileSync(join(root, "installer/AgentSessionManager.iss"), "[Setup]\n");
  const before = snapshot(root);
  assert.throws(
    () => synchronizeVersion(root, { version: "1.3.17" }),
    /expected exactly one/,
  );
  assert.deepEqual(snapshot(root), before);
});

test("rejects ambiguous metadata rather than updating unintended fields", (t) => {
  const root = fixture(t);
  const name = "src-tauri/tauri.windows.conf.json";
  writeFileSync(
    join(root, name),
    '{"app":{"windows":[{"title":"Agent会话管理器  v1.3.16"},{"title":"Agent会话管理器  v1.3.16"}]}}',
  );
  const before = snapshot(root);
  assert.throws(
    () => synchronizeVersion(root, { version: "1.3.17" }),
    /found 2/,
  );
  assert.deepEqual(snapshot(root), before);
});

function runCli(root, args) {
  const script = join(root, "scripts", "version.mjs");
  mkdirSync(dirname(script), { recursive: true });
  writeFileSync(
    script,
    readFileSync(new URL("./version.mjs", import.meta.url)),
  );
  // The caller's working directory must not change which project gets updated.
  return spawnSync(process.execPath, [script, ...args], {
    cwd: tmpdir(),
    encoding: "utf8",
  });
}

test("CLI works outside the repository cwd and synchronizes the fixture only", (t) => {
  const root = fixture(t);
  const check = runCli(root, ["--check"]);
  assert.equal(check.status, 0, check.stderr);
  assert.match(check.stdout, /Versions aligned: 1\.3\.16/);
  const update = runCli(root, ["1.3.17"]);
  assert.equal(update.status, 0, update.stderr);
  assert.match(update.stdout, /updated 6 files/);
  assert.equal(synchronizeVersion(root, { check: true }).version, "1.3.17");
});

test("CLI rejects invalid arguments and version drift with a nonzero exit code", (t) => {
  const root = fixture(t);
  const before = snapshot(root);
  for (const args of [[], ["--check", "extra"], ["1.3.17-beta.1"]]) {
    const result = runCli(root, args);
    assert.equal(result.status, 1, result.stderr);
  }
  assert.deepEqual(snapshot(root), before);
  writeFileSync(
    join(root, "installer/AgentSessionManager.iss"),
    files["installer/AgentSessionManager.iss"].replace("1.3.16", "1.0.0"),
  );
  const drifted = snapshot(root);
  const result = runCli(root, ["--check"]);
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, /Version mismatch/);
  assert.deepEqual(snapshot(root), drifted);
});
