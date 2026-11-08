import { useState } from "react";
import { useNavigate } from "react-router";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { AuthShell } from "@/components/AuthShell";
import { PasswordField, PASSWORD_MIN_LENGTH, PASSWORD_TOO_SHORT_MESSAGE } from "@/components/PasswordField";
import { api, apiErrorMessage } from "@/lib/api";
import { useScrubTokenParam } from "@/hooks/useScrubTokenParam";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

export function ResetPasswordPage() {
  useDocumentTitle("Reset password · arcodash");
  const token = useScrubTokenParam("token");
  const navigate = useNavigate();

  const [password, setPassword] = useState("");
  const [passwordError, setPasswordError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState(false);

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (password.length < PASSWORD_MIN_LENGTH) {
      setPasswordError(PASSWORD_TOO_SHORT_MESSAGE);
      return;
    }
    setError(null);
    setLoading(true);
    try {
      await api.resetPassword({ token, password });
      setSuccess(true);
    } catch (err) {
      setError(apiErrorMessage(err, "Reset failed"));
    } finally {
      setLoading(false);
    }
  }

  if (!token) {
    return (
      <AuthShell>
        <p className="text-destructive">Invalid or missing reset token.</p>
      </AuthShell>
    );
  }

  if (success) {
    return (
      <AuthShell>
        <Card className="w-full max-w-sm">
          <CardHeader>
            <CardTitle className="text-xl">Password Updated</CardTitle>
            <CardDescription>Your password has been changed successfully.</CardDescription>
          </CardHeader>
          <CardContent>
            <Button className="w-full" onClick={() => navigate("/login")}>
              Continue to Sign In
            </Button>
          </CardContent>
        </Card>
      </AuthShell>
    );
  }

  return (
    <AuthShell>
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle className="text-xl">Set New Password</CardTitle>
          <CardDescription>Choose a new password for your account.</CardDescription>
        </CardHeader>
        <CardContent>
          <form className="space-y-4" onSubmit={onSubmit}>
            <PasswordField
              idPrefix="rp-pw"
              label="New Password"
              value={password}
              onChange={setPassword}
              error={passwordError}
              onErrorClear={() => setPasswordError(null)}
              autoComplete="new-password"
            />
            {error && <p className="text-sm text-destructive">{error}</p>}
            <Button type="submit" className="w-full" disabled={loading}>
              {loading ? "Saving…" : "Set Password"}
            </Button>
          </form>
        </CardContent>
      </Card>
    </AuthShell>
  );
}
