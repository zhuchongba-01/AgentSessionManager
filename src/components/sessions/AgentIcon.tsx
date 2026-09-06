import openaiIcon from "@/icons/extracted/openai.svg?url";
import claudeIcon from "@/icons/extracted/claude.svg?url";
import grokIcon from "@/icons/extracted/grok.svg?url";
import opencodeIcon from "@/icons/extracted/opencode-logo-light.svg?url";
import zcodeIcon from "@/icons/extracted/zcode.png?url";
import piIcon from "@/icons/extracted/pi.svg?url";
import { cn } from "@/lib/utils";

const icons: Record<string, string> = {
  codex: openaiIcon,
  openai: openaiIcon,
  claude: claudeIcon,
  grok: grokIcon,
  grokbuild: grokIcon,
  opencode: opencodeIcon,
  zcode: zcodeIcon,
  pi: piIcon,
};

// 纯黑的单色 logo：亮色主题直接用，暗色主题由 CSS 反色成白，
// 否则黑标贴在暗背景上不可见。
const monoIcons = new Set([openaiIcon, grokIcon]);

// 各家 logo 在自身画布里的占比差异很大（pi 的标记只占约六成，
// zcode/opencode 是满幅方块），按倍率做光学补偿，让实际观感一致：
// 名义 16px 时所有图标的视觉高度都对齐在 ~14px。
const opticalScale: Record<string, number> = {
  // 基准 = codex（OpenAI 标 1.1 时视觉约 16px）；各品牌按标记占画布比例
  // 换算到同一视觉高度，保证列表里所有行图标观感一致
  openai: 1.1,
  claude: 1.1,
  grok: 1.15,
  opencode: 1.0,
  zcode: 1.1,
  pi: 1.7,
};

export function AgentIcon({
  icon,
  name,
  size = 18,
  className,
}: {
  icon?: string;
  name: string;
  size?: number;
  className?: string;
}) {
  const key = (icon || name).toLocaleLowerCase();
  const source = icons[key];
  if (source) {
    const scale = opticalScale[key] ?? 1;
    return (
      <img
        src={source}
        alt=""
        title={name}
        className={cn(
          "inline-block shrink-0 object-contain",
          monoIcons.has(source) && "asm-icon-mono",
          className,
        )}
        style={{ width: size, height: size, transform: `scale(${scale})` }}
      />
    );
  }

  return (
    <span
      aria-hidden="true"
      title={name}
      className={cn(
        "inline-grid shrink-0 place-items-center rounded-md bg-muted font-semibold text-muted-foreground",
        className,
      )}
      style={{ width: size, height: size, fontSize: Math.max(9, size * 0.55) }}
    >
      {name.slice(0, 1).toLocaleUpperCase()}
    </span>
  );
}
