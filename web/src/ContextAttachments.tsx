import { useRef, useState } from "react";

export type ContextAttachment = { name: string; content: string };
const MAX_FILES = 5;
const MAX_FILE_BYTES = 32 * 1024;
const MAX_TOTAL_BYTES = 96 * 1024;
const bytes = (value: string) => new TextEncoder().encode(value).length;

export function ContextAttachments({
  value,
  onChange,
  disabled,
  onReadingChange,
}: {
  value: ContextAttachment[];
  onChange: (files: ContextAttachment[]) => void;
  disabled: boolean;
  onReadingChange: (reading: boolean) => void;
}) {
  const input = useRef<HTMLInputElement>(null);
  const reading = useRef(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [dragging, setDragging] = useState(false);
  async function addFiles(files: File[]) {
    if (disabled || reading.current || !files.length) return;
    setError("");
    reading.current = true;
    setBusy(true);
    onReadingChange(true);
    try {
      if (value.length + files.length > MAX_FILES)
        throw new Error(
          "Attach up to five files. Remove a file before adding more.",
        );
      const names = new Set(value.map((file) => file.name.toLowerCase()));
      let total = value.reduce((sum, file) => sum + bytes(file.content), 0);
      const additions: ContextAttachment[] = [];
      for (const file of files) {
        if (!/\.(txt|json|md)$/i.test(file.name))
          throw new Error("Choose .txt, .json or .md files only.");
        if (bytes(file.name) > 160)
          throw new Error(
            "A file name is too long. Shorten it to 160 UTF-8 bytes or fewer.",
          );
        if (names.has(file.name.toLowerCase()))
          throw new Error(
            "A file with that name is already selected. Rename it or remove the existing file.",
          );
        if (file.size > MAX_FILE_BYTES)
          throw new Error("Each file must be 32 KiB or smaller.");
        let content: string;
        try {
          content = new TextDecoder("utf-8", { fatal: true }).decode(
            await file.arrayBuffer(),
          );
        } catch {
          throw new Error(
            "Files must contain valid UTF-8 text. Convert the file and try again.",
          );
        }
        if (
          !content.trim() ||
          /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(content)
        )
          throw new Error(
            "Files must contain readable text, without binary or control characters.",
          );
        if (/\.json$/i.test(file.name)) {
          try {
            JSON.parse(content);
          } catch {
            throw new Error(
              "A JSON file is not valid. Correct its JSON and try again.",
            );
          }
        }
        total += bytes(content);
        if (total > MAX_TOTAL_BYTES)
          throw new Error("The selected files must total 96 KiB or less.");
        names.add(file.name.toLowerCase());
        additions.push({ name: file.name, content });
      }
      onChange([...value, ...additions]);
    } catch (failure) {
      setError(
        failure instanceof Error
          ? failure.message
          : "The files could not be read. Try again.",
      );
    } finally {
      reading.current = false;
      setBusy(false);
      onReadingChange(false);
      if (input.current) input.current.value = "";
    }
  }
  return (
    <section
      className="context-attachments"
      aria-labelledby="attachments-title"
    >
      <h3 id="attachments-title">Attachment for the agent</h3>
      <input
        ref={input}
        className="attachment-input"
        type="file"
        multiple
        accept=".txt,.json,.md"
        aria-label="Attach context files"
        disabled={disabled || busy}
        onChange={(event) =>
          void addFiles(Array.from(event.currentTarget.files ?? []))
        }
      />
      <button
        type="button"
        className={`attachment-dropzone ${dragging ? "dragging" : ""}`}
        disabled={disabled || busy}
        aria-describedby="attachment-limits attachment-use"
        onClick={() => input.current?.click()}
        onDragOver={(event) => {
          event.preventDefault();
          if (!disabled && !busy) setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(event) => {
          event.preventDefault();
          setDragging(false);
          void addFiles(Array.from(event.dataTransfer.files));
        }}
      >
        <span>
          {busy
            ? "Reading files…"
            : value.length
              ? `${value.length} file${value.length === 1 ? "" : "s"} selected · .txt, .json, .md`
              : "None selected. Supports .txt, .json, .md files"}
        </span>
        <strong>Drop or attach files</strong>
      </button>
      {!!value.length && (
        <ul className="attachment-list" aria-label="Selected context files">
          {value.map((file) => (
            <li key={file.name}>
              <span>
                <strong>{file.name}</strong>
                <span className="small muted">
                  {bytes(file.content) < 1024
                    ? `${bytes(file.content)} B`
                    : `${(bytes(file.content) / 1024).toFixed(1)} KiB`}
                </span>
              </span>
              <button
                type="button"
                className="quiet"
                disabled={disabled || busy}
                aria-label={`Remove ${file.name}`}
                onClick={() => {
                  onChange(value.filter((item) => item !== file));
                  setError("");
                }}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
      {error && (
        <div role="alert" className="notice error">
          {error} Your other selected files are unchanged.
        </div>
      )}
      <p id="attachment-limits" className="small muted">
        Optional · Up to 5 UTF-8 text files, 32 KiB each, 96 KiB total.
      </p>
      <p id="attachment-use" className="small muted">
        Files and project context are processed by AI to guide the interview.
        Files are not shown on the invitation, but their content may influence
        what the interviewer asks.
      </p>
    </section>
  );
}
