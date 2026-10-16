import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Tauri 的 CSP 用 sha256 白名单放行 `src/index.html` 里的内联脚本。该脚本负责
 * 首屏主题预置；一旦哈希与内联内容不匹配，脚本会被 CSP 静默拦掉，用户看到的是
 * 主题闪烁而不是报错——所以这里把它做成可校验的门禁，而不是靠人肉复制粘贴。
 */
const INLINE_SCRIPT = /<script>([\s\S]*?)<\/script>/g;
const CSP_SHA256 = /'sha256-([A-Za-z0-9+/=]+)'/;

export function inlineScriptDigests(html) {
  const digests = [];
  for (const match of html.matchAll(INLINE_SCRIPT)) {
    digests.push(
      createHash("sha256").update(match[1], "utf8").digest("base64"),
    );
  }
  return digests;
}

/**
 * 校验 `src/index.html` 的每个内联脚本是否都在 `tauri.conf.json` 的 CSP 里。
 * 返回 `{ ok, missing, configured }`；不写文件。
 */
export function checkCspHashes(root) {
  const html = readFileSync(resolve(root, "src/index.html"), "utf8");
  const config = readFileSync(resolve(root, "src-tauri/tauri.conf.json"), "utf8");

  const csp = JSON.parse(config)?.app?.security?.csp;
  if (typeof csp !== "string") {
    throw new Error("tauri.conf.json: app.security.csp is missing");
  }
  const configured = [...csp.matchAll(/'sha256-([A-Za-z0-9+/=]+)'/g)].map(
    (match) => match[1],
  );
  if (configured.length === 0) {
    throw new Error("tauri.conf.json: CSP declares no sha256 source");
  }

  const required = inlineScriptDigests(html);
  if (required.length === 0) {
    throw new Error("src/index.html: no inline script found to hash");
  }

  const missing = required.filter((digest) => !configured.includes(digest));
  return { ok: missing.length === 0, required, configured, missing };
}

const scriptPath = fileURLToPath(import.meta.url);
if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    const root = resolve(dirname(scriptPath), "..");
    const result = checkCspHashes(root);
    if (!result.ok) {
      console.error(
        `CSP is missing an inline-script sha256. Add: ${result.missing
          .map((digest) => `'sha256-${digest}'`)
          .join(", ")}`,
      );
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
