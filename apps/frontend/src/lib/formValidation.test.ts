import { describe, it, expect } from "vitest";
import { fieldKind, validateValue } from "./formValidation";

describe("fieldKind", () => {
  it("recognizes a bare 'port' key regardless of declared type", () => {
    expect(fieldKind("port", "integer")).toBe("port");
    expect(fieldKind("port", "string")).toBe("port");
  });

  it("recognizes any key containing 'url'", () => {
    expect(fieldKind("url", "string")).toBe("url");
    expect(fieldKind("api_endpoint_url", "string")).toBe("url");
  });

  it("recognizes email-shaped keys", () => {
    expect(fieldKind("to", "string")).toBe("email");
    expect(fieldKind("addresses", "string")).toBe("email");
    expect(fieldKind("contact_email", "string")).toBe("email");
  });

  it("returns undefined for a plain field", () => {
    expect(fieldKind("host", "string")).toBeUndefined();
    expect(fieldKind("dbname", "string")).toBeUndefined();
  });
});

describe("validateValue", () => {
  it("required + empty wins over any kind-specific check", () => {
    expect(validateValue("Name", true, undefined, "")).toBe("Name is required.");
    expect(validateValue("Name", true, undefined, "   ")).toBe("Name is required.");
  });

  it("optional + empty is valid", () => {
    expect(validateValue("Site", false, "url", "")).toBeNull();
  });

  it("port: valid range passes, out of range and non-numeric fail with the same message", () => {
    expect(validateValue("Port", true, "port", "5432")).toBeNull();
    expect(validateValue("Port", true, "port", "1")).toBeNull();
    expect(validateValue("Port", true, "port", "65535")).toBeNull();
    expect(validateValue("Port", true, "port", "0")).toBe("Enter a port between 1 and 65535.");
    expect(validateValue("Port", true, "port", "65536")).toBe(
      "Enter a port between 1 and 65535.",
    );
    expect(validateValue("Port", true, "port", "abc")).toBe(
      "Enter a port between 1 and 65535.",
    );
  });

  it("url: requires an http(s) scheme", () => {
    expect(validateValue("Webhook URL", true, "url", "https://hooks.slack.com/x")).toBeNull();
    expect(validateValue("Webhook URL", true, "url", "http://example.com")).toBeNull();
    expect(validateValue("Webhook URL", true, "url", "ftp://example.com")).toBe(
      "Enter a URL starting with http:// or https://.",
    );
    expect(validateValue("Webhook URL", true, "url", "example.com")).toBe(
      "Enter a URL starting with http:// or https://.",
    );
  });

  it("email: validates a single address and every entry in a comma-separated list", () => {
    expect(validateValue("To", true, "email", "sam@x.io")).toBeNull();
    expect(validateValue("To", true, "email", "sam@x.io, joe@x.io")).toBeNull();
    expect(validateValue("To", true, "email", "sam@x.io, not-an-email")).toBe(
      "Enter a valid email address.",
    );
    expect(validateValue("To", true, "email", "not-an-email")).toBe(
      "Enter a valid email address.",
    );
  });
});
