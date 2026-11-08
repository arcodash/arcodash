import { useState } from "react";
import { Eye, EyeOff } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

export const PASSWORD_MIN_LENGTH = 8;
export const PASSWORD_TOO_SHORT_MESSAGE = "Password must be at least 8 characters.";

interface PasswordFieldProps {
  /** Prefixes every id this field renders; keep unique per page. */
  idPrefix: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  error: string | null;
  onErrorClear: () => void;
  autoComplete: "new-password" | "current-password";
}

/** Shared password-entry pattern: static "at least 8 characters" hint, show/hide toggle, and an
 * error that re-validates per keystroke once shown. `minLength` is deliberately not set on the
 * `<input>`: its native tooltip is unstyleable, so length is validated in JS. */
export function PasswordField({
  idPrefix,
  label,
  value,
  onChange,
  error,
  onErrorClear,
  autoComplete,
}: PasswordFieldProps) {
  const [visible, setVisible] = useState(false);
  const hintId = `${idPrefix}-hint`;
  const errorId = `${idPrefix}-error`;

  return (
    <div className="space-y-2">
      <Label htmlFor={idPrefix}>{label}</Label>
      <div className="relative">
        <Input
          id={idPrefix}
          type={visible ? "text" : "password"}
          autoComplete={autoComplete}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          value={value}
          onChange={(e) => {
            const next = e.target.value;
            onChange(next);
            if (error && next.length >= PASSWORD_MIN_LENGTH) onErrorClear();
          }}
          required
          aria-invalid={!!error}
          aria-describedby={error ? `${hintId} ${errorId}` : hintId}
          className="pr-10"
        />
        <button
          type="button"
          className="absolute inset-y-0 right-0 flex items-center px-3 text-muted-foreground hover:text-foreground cursor-pointer"
          onClick={() => setVisible((v) => !v)}
          aria-label={visible ? "Hide password" : "Show password"}
        >
          {visible ? (
            <EyeOff className="h-4 w-4" aria-hidden="true" />
          ) : (
            <Eye className="h-4 w-4" aria-hidden="true" />
          )}
        </button>
      </div>
      <p id={hintId} className="text-sm text-muted-foreground">
        At least 8 characters.
      </p>
      {error && (
        <p id={errorId} role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
