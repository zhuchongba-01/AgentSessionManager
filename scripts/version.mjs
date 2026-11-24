import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Only explicit release metadata is edited. Dependency versions are never touched.
const windowTitle = /("title"\s*:\s*"Agent会话管理\s+v)([^"\r\n]+)(")/g;

/**
 * Locate top-level `"version": "..."` string properties on a root JSON object
 * (depth 1) without matching nested `"version"` keys inside child objects/arrays,
 * while preserving exact surrounding formatting and line endings.
 */
function findTopLevelJsonVersionSpans(text, relativePath) {
  try {
    JSON.parse(text);
  } catch (error) {
    throw new Error(`${relativePath}: invalid JSON (${error.message})`);
  }

  const spans = [];
  const len = text.length;
  let i = 0;
  let braceDepth = 0;
  let bracketDepth = 0;
  let expectingKey = false;

  const skipWhitespace = () => {
    while (
      i < len &&
      (text[i] === " " ||
        text[i] === "\t" ||
        text[i] === "\r" ||
        text[i] === "\n")
    ) {
      i++;
    }
  };

  const readJsonString = () => {
    const startQuote = i;
    i++;
    while (i < len) {
      const ch = text[i];
      if (ch === "\\") {
        i += 2;
        continue;
      }
      if (ch === '"') {
        const endQuote = i;
        i++;
        return {
          raw: text.slice(startQuote, i),
          valueStart: startQuote + 1,
          valueEnd: endQuote,
        };
      }
      i++;
    }
    throw new Error(`${relativePath}: unterminated JSON string`);
  };

  while (i < len) {
    skipWhitespace();
    if (i >= len) break;
    const ch = text[i];

    if (ch === "{") {
      braceDepth++;
      expectingKey = braceDepth === 1 && bracketDepth === 0;
      i++;
    } else if (ch === "}") {
      braceDepth--;
      expectingKey = false;
      i++;
    } else if (ch === "[") {
      bracketDepth++;
      expectingKey = false;
      i++;
    } else if (ch === "]") {
      bracketDepth--;
      expectingKey = false;
      i++;
    } else if (ch === ",") {
      expectingKey = braceDepth === 1 && bracketDepth === 0;
      i++;
    } else if (ch === '"') {
      const isTopLevelKey =
        expectingKey && braceDepth === 1 && bracketDepth === 0;
      const keyToken = readJsonString();
      expectingKey = false;

      if (isTopLevelKey) {
        skipWhitespace();
        if (text[i] === ":") {
          i++;
          skipWhitespace();
          if (JSON.parse(keyToken.raw) === "version" && text[i] === '"') {
            const valToken = readJsonString();
            const rawInner = text.slice(valToken.valueStart, valToken.valueEnd);
            if (!/[\r\n"]/.test(rawInner)) {
              spans.push({
                start: valToken.valueStart,
                end: valToken.valueEnd,
              });
            }
          }
        }
      }
    } else {
      expectingKey = false;
      i++;
    }
  }

  return spans;
}

function replaceTopLevelJsonVersion(text, version, relativePath) {
  const spans = findTopLevelJsonVersionSpans(text, relativePath);
  if (spans.length !== 1) {
    throw new Error(
      `${relativePath}: expected exactly one release metadata field, found ${spans.length}`,
    );
  }
  const [{ start, end }] = spans;
  return `${text.slice(0, start)}${version}${text.slice(end)}`;
}

const targets = [
  ["package.json", [replaceTopLevelJsonVersion]],
  [
    "src-tauri/Cargo.toml",
    [
      /^(\[package\]\r?\nname = "agent-session-manager"\r?\nversion = ")([^"\r\n]+)(")/gm,
    ],
  ],
  [
    "src-tauri/Cargo.lock",
    [
      /^(\[\[package\]\]\r?\nname = "agent-session-manager"\r?\nversion = ")([^"\r\n]+)(")/gm,
    ],
  ],
  ["src-tauri/tauri.conf.json", [replaceTopLevelJsonVersion, windowTitle]],
  ["src-tauri/tauri.windows.conf.json", [windowTitle]],
  ["src-tauri/tauri.macos.conf.json", [windowTitle]],
  [
    "installer/AgentSessionManager.iss",
    [/^(#define MyAppVersion ")([^"\r\n]+)(")/gm],
  ],
];

function validateVersion(version) {
  if (
    typeof version !== "string" ||
    version.trim() !== version ||
    !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)
  ) {
    throw new Error("Expected a stable version such as 1.3.17 (no v prefix).");
  }
}

/** Check with package.json as the source of truth, or synchronize a new version. */
export function synchronizeVersion(root, { check = false, version } = {}) {
  if (check) {
    version = JSON.parse(
      readFileSync(resolve(root, "package.json"), "utf8"),
    ).version;
  }
  validateVersion(version);

  // Validate every expected field before writing anything; preserve formatting/EOLs.
  const plan = targets.map(([relativePath, patterns]) => {
    const path = resolve(root, relativePath);
    const before = readFileSync(path, "utf8");
    let after = before;
    for (const pattern of patterns) {
      if (typeof pattern === "function") {
        after = pattern(after, version, relativePath);
        continue;
      }
      const matches = [...after.matchAll(pattern)];
      if (matches.length !== 1) {
        throw new Error(
          `${relativePath}: expected exactly one release metadata field, found ${matches.length}`,
        );
      }
      after = after.replace(
        pattern,
        (_match, prefix, _oldVersion, suffix) => `${prefix}${version}${suffix}`,
      );
    }
    return { path, relativePath, before, after };
  });
  const changed = plan.filter(({ before, after }) => before !== after);
  if (check && changed.length > 0) {
    throw new Error(
      `Version mismatch (expected ${version}): ${changed.map(({ relativePath }) => relativePath).join(", ")}`,
    );
  }
  if (!check) {
    const attempted = [];
    try {
      for (const item of changed) {
        attempted.push(item);
        writeFileSync(item.path, item.after, "utf8");
      }
    } catch (error) {
      // Best-effort rollback on write failures; this is not a crash-safe transaction.
      const failures = [];
      for (const item of attempted.reverse()) {
        try {
          writeFileSync(item.path, item.before, "utf8");
        } catch {
          failures.push(item.relativePath);
        }
      }
      if (failures.length > 0) {
        throw new Error(
          `Version update failed; restore these files from Git: ${failures.join(", ")}`,
          { cause: error },
        );
      }
      throw error;
    }
  }
  return { version, changed: changed.map(({ relativePath }) => relativePath) };
}

const scriptPath = fileURLToPath(import.meta.url);
if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    const args = process.argv.slice(2);
    if (args.length !== 1) {
      throw new Error("Usage: node scripts/version.mjs <X.Y.Z|--check>");
    }
    const check = args[0] === "--check";
    const result = synchronizeVersion(resolve(dirname(scriptPath), ".."), {
      check,
      version: args[0],
    });
    console.log(
      check
        ? `Versions aligned: ${result.version}`
        : `Version ${result.version}: updated ${result.changed.length} files`,
    );
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
