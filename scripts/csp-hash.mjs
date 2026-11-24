import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Tauri 的 CSP 用 sha256 白名单放行 `src/index.html` 里的内联脚本。该脚本负责
 * 首屏主题预置；一旦哈希与内联内容不匹配，脚本会被 CSP 静默拦掉，用户看到的是
 * 主题闪烁而不是报错——所以这里把它做成可校验的门禁，而不是靠人肉复制粘贴。
 */
const INLINE_SCRIPT = /<script(?![^>]*(?<![-\w])src\s*=)[^>]*>([\s\S]*?)<\/script>/gi;

export function inlineScriptDigests(html) {
  const digests = [];
  for (const match of html.matchAll(INLINE_SCRIPT)) {
    digests.push(
      createHash("sha256").update(match[1], "utf8").digest("base64"),
    );
  }
  return digests;
}

function parseScriptSrcDigests(csp) {
  const directives = csp
    .split(";")
    .map((part) => part.trim())
    .filter(Boolean);

  const scriptSrcDirectives = directives.filter((directive) => {
    const [name] = directive.split(/\s+/);
    return name.toLowerCase() === "script-src";
  });

  if (scriptSrcDirectives.length === 0) {
    throw new Error("tauri.conf.json: CSP script-src declares no sha256 source");
  }
  if (scriptSrcDirectives.length > 1) {
    throw new Error("tauri.conf.json: CSP declares duplicate script-src directives");
  }

  const tokens = scriptSrcDirectives[0].split(/\s+/).slice(1);
  const digests = [];
  for (const token of tokens) {
    const match = /^'sha256-([A-Za-z0-9+/=]+)'$/.exec(token);
    if (match) {
      digests.push(match[1]);
    } else if (/sha256-/i.test(token)) {
      throw new Error(
        `tauri.conf.json: malformed sha256 source in script-src: ${token}`,
      );
    }
  }

  if (digests.length === 0) {
    throw new Error("tauri.conf.json: CSP script-src declares no sha256 source");
  }
  return digests;
}

/**
 * 校验 `src/index.html` 的每个内联脚本是否都与 `tauri.conf.json` 的 `script-src`
 * sha256 列表严格对齐（既不缺失，也不残留多余旧哈希）。
 * 返回 `{ ok, required, configured, missing, extra }`；不写文件。
 */
export function checkCspHashes(root) {
  const html = readFileSync(resolve(root, "src/index.html"), "utf8");
  const config = readFileSync(resolve(root, "src-tauri/tauri.conf.json"), "utf8");

  const csp = JSON.parse(config)?.app?.security?.csp;
  if (typeof csp !== "string") {
    throw new Error("tauri.conf.json: app.security.csp is missing");
  }
  const configured = parseScriptSrcDigests(csp);

  const required = inlineScriptDigests(html);
  if (required.length === 0) {
    throw new Error("src/index.html: no inline script found to hash");
  }

  const missing = required.filter((digest) => !configured.includes(digest));
  const seen = new Set();
  const extra = [];
  for (const digest of configured) {
    if (!required.includes(digest) || seen.has(digest)) {
      extra.push(digest);
    }
    seen.add(digest);
  }

  return {
    ok: missing.length === 0 && extra.length === 0,
    required,
    configured,
    missing,
    extra,
  };
}

const scriptPath = fileURLToPath(import.meta.url);
if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    const root = resolve(dirname(scriptPath), "..");
    const result = checkCspHashes(root);
    if (!result.ok) {
      if (result.missing.length > 0) {
        console.error(
          `CSP is missing an inline-script sha256. Add: ${result.missing
            .map((digest) => `'sha256-${digest}'`)
            .join(", ")}`,
        );
      }
      if (result.extra.length > 0) {
        console.error(
          `CSP has stale or extra script-src sha256 hash(es). Remove: ${result.extra
            .map((digest) => `'sha256-${digest}'`)
            .join(", ")}`,
        );
      }
      process.exitCode = 1;
    } else {
      console.log(
        `CSP covers ${result.required.length} inline script(s): ${result.required
          .map((digest) => `sha256-${digest}`)
          .join(", ")}`,
      );
    }
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
