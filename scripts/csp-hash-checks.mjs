import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { checkCspHashes, inlineScriptDigests } from "./csp-hash.mjs";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const sha256 = (text) =>
  createHash("sha256").update(text, "utf8").digest("base64");

function fixture({ html, csp }) {
  const root = mkdtempSync(join(tmpdir(), "asm-csp-"));
  mkdirSync(join(root, "src"), { recursive: true });
  mkdirSync(join(root, "src-tauri"), { recursive: true });
  writeFileSync(join(root, "src/index.html"), html);
  writeFileSync(
    join(root, "src-tauri/tauri.conf.json"),
    JSON.stringify({ app: { security: { csp } } }),
  );
  return root;
}

test("the shipped inline script hash matches the shipped CSP", () => {
  // 这条断言直接盯着仓库当前状态：改了 index.html 内联脚本却忘了更新 CSP，
  // 会在 CI 里失败，而不是等用户看到主题闪烁。
  const result = checkCspHashes(projectRoot);
  assert.equal(result.ok, true, `missing: ${result.missing.join(", ")}`);
  assert.equal(result.required.length, 1);
});

test("digests only inline scripts, ignoring src scripts", () => {
  const inline = "(function(){var a=1})()";
  const html = `<script src="./main.tsx"></script><script>${inline}</script>`;
  assert.deepEqual(inlineScriptDigests(html), [sha256(inline)]);
});

test("accepts a CSP that covers every inline script", (t) => {
  const inline = "(function(){var a=1})()";
  const root = fixture({
    html: `<script>${inline}</script>`,
    csp: `default-src 'self'; script-src 'self' 'sha256-${sha256(inline)}'`,
  });
  t.after(() => rmSync(root, { recursive: true, force: true }));

  const result = checkCspHashes(root);
  assert.equal(result.ok, true);
  assert.deepEqual(result.missing, []);
});

test("reports a stale CSP hash instead of silently accepting it", (t) => {
  const root = fixture({
    html: "<script>(function(){var a=1})()</script>",
    csp: "default-src 'self'; script-src 'self' 'sha256-c3ThbGUK'",
  });
  t.after(() => rmSync(root, { recursive: true, force: true }));

  const result = checkCspHashes(root);
  assert.equal(result.ok, false);
  assert.equal(result.missing.length, 1);
});

test("fails closed when the CSP declares no sha256 source", (t) => {
  const root = fixture({
    html: "<script>(function(){})()</script>",
    csp: "default-src 'self'; script-src 'self'",
  });
  t.after(() => rmSync(root, { recursive: true, force: true }));

  assert.throws(() => checkCspHashes(root), /no sha256 source/);
});

test("CLI resolves the project from its own location and reports drift", (t) => {
  // CLI 以脚本自身位置定位项目（与 version.mjs 一致），所以这里在临时目录里
  // 放一份脚本副本 + 项目副本，验证 cwd 不影响结果。
  const fakeRoot = mkdtempSync(join(tmpdir(), "asm-csp-cli-"));
  t.after(() => rmSync(fakeRoot, { recursive: true, force: true }));
  mkdirSync(join(fakeRoot, "scripts"), { recursive: true });
  mkdirSync(join(fakeRoot, "src"), { recursive: true });
  mkdirSync(join(fakeRoot, "src-tauri"), { recursive: true });
  writeFileSync(
    join(fakeRoot, "scripts/csp-hash.mjs"),
    readFileSync(new URL("./csp-hash.mjs", import.meta.url)),
  );
  writeFileSync(
    join(fakeRoot, "src/index.html"),
    readFileSync(join(projectRoot, "src/index.html")),
  );
  writeFileSync(
    join(fakeRoot, "src-tauri/tauri.conf.json"),
    readFileSync(join(projectRoot, "src-tauri/tauri.conf.json")),
  );

  const run = () =>
    spawnSync(process.execPath, [join(fakeRoot, "scripts/csp-hash.mjs")], {
      cwd: tmpdir(),
      encoding: "utf8",
    });

  const aligned = run();
  assert.equal(aligned.status, 0, aligned.stderr);
  assert.match(aligned.stdout, /CSP covers 1 inline script/);

  writeFileSync(
    join(fakeRoot, "src-tauri/tauri.conf.json"),
    JSON.stringify({
      app: { security: { csp: "script-src 'self' 'sha256-c3ThbGUK'" } },
    }),
  );
  const drifted = run();
  assert.equal(drifted.status, 1, drifted.stdout);
  assert.match(drifted.stderr, /missing an inline-script sha256/);
});

test("the repository index.html is not accidentally empty", () => {
  const html = readFileSync(join(projectRoot, "src/index.html"), "utf8");
  assert.match(html, /localStorage\.getItem\("agent-session-manager-theme"\)/);
});
