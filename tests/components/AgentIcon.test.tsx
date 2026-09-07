import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { AgentIcon } from "@/components/sessions/AgentIcon";

describe("AgentIcon", () => {
  it("keeps every brand in the same layout slot", () => {
    render(
      <>
        <AgentIcon icon="openai" name="Codex" size={16} />
        <AgentIcon icon="pi" name="Pi" size={16} />
      </>,
    );
    for (const icon of [screen.getByTitle("Codex"), screen.getByTitle("Pi")]) {
      expect(icon).toHaveStyle({ width: "16px", height: "16px" });
    }
  });
});
