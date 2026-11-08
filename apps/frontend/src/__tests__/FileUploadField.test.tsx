import { describe, it, expect, vi, afterEach } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { FileUploadField } from "@/components/data-sources/FileUploadField";
import type { UploadDataSourceResponse } from "@/types/api";

vi.mock("@/lib/api", () => {
  class ApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.name = "ApiError";
      this.status = status;
    }
  }
  return {
    ApiError,
    api: {
      uploadDataSourceFile: vi.fn(),
    },
  };
});

import { ApiError, api } from "@/lib/api";

const mockApi = api as unknown as {
  uploadDataSourceFile: ReturnType<typeof vi.fn>;
};

function makeResult(overrides: Partial<UploadDataSourceResponse> = {}): UploadDataSourceResponse {
  return {
    upload_id: "11111111-1111-1111-1111-111111111111",
    filename: "orders.csv",
    byte_size: 2048,
    content_type: "text/csv",
    columns: [{ name: "id", friendly_type: "string" }],
    preview_rows: [{ id: "1" }],
    ...overrides,
  };
}

function selectFile(input: HTMLElement, file: File) {
  fireEvent.change(input, { target: { files: [file] } });
}

describe("FileUploadField", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("goes idle -> uploading -> success and reports the upload_id via onUploaded", async () => {
    let resolveUpload!: (r: UploadDataSourceResponse) => void;
    mockApi.uploadDataSourceFile.mockReturnValue(
      new Promise<UploadDataSourceResponse>((resolve) => {
        resolveUpload = resolve;
      }),
    );
    const onUploaded = vi.fn();

    render(
      <FileUploadField connectorType="csv" fieldTitle="CSV file" accept=".csv" onUploaded={onUploaded} />,
    );

    expect(screen.getByTestId("file-upload-dropzone")).toBeInTheDocument();

    const file = new File(["id\n1"], "orders.csv", { type: "text/csv" });
    selectFile(screen.getByTestId("file-upload-input"), file);

    expect(await screen.findByText("orders.csv")).toBeInTheDocument();

    const result = makeResult();
    await act(async () => {
      resolveUpload(result);
    });

    expect(await screen.findByText(/1 columns/)).toBeInTheDocument();
    expect(screen.getByText("id")).toBeInTheDocument();
    expect(onUploaded).toHaveBeenCalledWith(expect.objectContaining({ upload_id: result.upload_id }));
  });

  it("shows 'Current: <filename>' in edit mode, and Replace reveals the drop zone", async () => {
    render(
      <FileUploadField
        connectorType="sqlite"
        fieldTitle="SQLite database"
        accept=".sqlite,.db,.sqlite3"
        currentFilename="warehouse.sqlite"
        onUploaded={vi.fn()}
      />,
    );

    expect(screen.getByText(/Current:/)).toBeInTheDocument();
    expect(screen.getByText("warehouse.sqlite")).toBeInTheDocument();
    expect(screen.queryByTestId("file-upload-dropzone")).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Replace" }));

    expect(screen.getByTestId("file-upload-dropzone")).toBeInTheDocument();
  });

  it("shows an inline error and returns to the drop zone when the server rejects the upload", async () => {
    mockApi.uploadDataSourceFile.mockRejectedValue(new ApiError(400, "could not parse CSV"));

    render(<FileUploadField connectorType="csv" fieldTitle="CSV file" accept=".csv" onUploaded={vi.fn()} />);

    const file = new File(["not a csv"], "bad.csv", { type: "text/csv" });
    selectFile(screen.getByTestId("file-upload-input"), file);

    expect(await screen.findByText("could not parse CSV")).toBeInTheDocument();
    expect(screen.getByTestId("file-upload-dropzone")).toBeInTheDocument();
  });

  it("rejects an oversized file client-side without calling the API", async () => {
    render(<FileUploadField connectorType="csv" fieldTitle="CSV file" accept=".csv" onUploaded={vi.fn()} />);

    const bigFile = new File(["x"], "huge.csv", { type: "text/csv" });
    Object.defineProperty(bigFile, "size", { value: 26 * 1024 * 1024 });
    selectFile(screen.getByTestId("file-upload-input"), bigFile);

    expect(await screen.findByText(/too large/)).toBeInTheDocument();
    expect(mockApi.uploadDataSourceFile).not.toHaveBeenCalled();
  });
});
