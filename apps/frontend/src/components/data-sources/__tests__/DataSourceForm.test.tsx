import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { DataSourceForm } from "../DataSourceForm";
import type { DataSourceType } from "@/types/api";

const SNOWFLAKE_TYPE: DataSourceType = {
  type: "snowflake",
  name: "Snowflake",
  configuration_schema: {
    properties: {
      private_key: { type: "string", title: "Private Key", "x-widget": "textarea" },
      private_key_passphrase: { type: "string", title: "Private Key Passphrase" },
    },
    secret: ["private_key", "private_key_passphrase"],
  },
};

describe("DataSourceForm", () => {
  it("renders x-widget textarea fields multi-line so pasted PEM keys keep their newlines", async () => {
    const onSubmit = vi.fn();
    render(<DataSourceForm types={[SNOWFLAKE_TYPE]} presetType="snowflake" onSubmit={onSubmit} />);

    const key = screen.getByLabelText("Private Key");
    expect(key.tagName).toBe("TEXTAREA");
    expect(screen.getByLabelText("Private Key Passphrase")).toHaveAttribute("type", "password");

    const pem = "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIF\n-----END ENCRYPTED PRIVATE KEY-----";
    await userEvent.click(key);
    await userEvent.paste(pem);
    await userEvent.type(screen.getByLabelText(/^Name/), "sf");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));

    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({ options: expect.objectContaining({ private_key: pem }) }),
    );
  });
});
