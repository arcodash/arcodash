import { useState, type FormEvent } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { PageHeader } from "@/components/common/PageHeader";
import { ReadonlyField } from "@/components/common/ReadonlyField";
import { PageLoader } from "@/components/ui/loader";
import { api, apiErrorMessage } from "@/lib/api";
import { useAuth, SESSION_QUERY_KEY } from "@/lib/auth";
import { CREDENTIAL_CHECK_MUTATION_KEY } from "@/lib/credentialCheckMutationKey";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

type Message = { kind: "success" | "error"; text: string } | null;

function StatusMessage({ message }: { message: Message }) {
  if (!message) return null;
  return (
    <p className={`text-sm ${message.kind === "success" ? "text-green-600" : "text-destructive"}`}>{message.text}</p>
  );
}

export function AccountPage() {
  const { user, clientConfig, isLoading } = useAuth();
  const queryClient = useQueryClient();
  useDocumentTitle("Account · arcodash");

  const [name, setName] = useState(user?.name ?? "");
  const [nameMessage, setNameMessage] = useState<Message>(null);

  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");
  const [passwordMessage, setPasswordMessage] = useState<Message>(null);

  const nameMutation = useMutation({
    mutationFn: (nextName: string) => api.updateUser(user!.id, { name: nextName }),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      setNameMessage({ kind: "success", text: "Name updated." });
    },
    onError: (err) => setNameMessage({ kind: "error", text: apiErrorMessage(err, "Couldn't update your name.") }),
  });

  const passwordMutation = useMutation({
    // A 401 here means the current password was wrong, not that the session expired — exempt
    // it from the app-wide sign-out handler so the error surfaces inline instead.
    mutationKey: [CREDENTIAL_CHECK_MUTATION_KEY, "changePassword"],
    mutationFn: (body: { current_password: string; new_password: string }) => api.changePassword(body),
    onSuccess: () => {
      setCurrentPassword("");
      setNewPassword("");
      setConfirmPassword("");
      setPasswordMessage({ kind: "success", text: "Password changed." });
    },
    onError: (err) =>
      setPasswordMessage({ kind: "error", text: apiErrorMessage(err, "Couldn't change your password.") }),
  });

  if (isLoading) return <PageLoader />;
  if (!user) return null;

  function onSubmitName(e: FormEvent) {
    e.preventDefault();
    setNameMessage(null);
    const trimmed = name.trim();
    if (!trimmed) return;
    nameMutation.mutate(trimmed);
  }

  function onSubmitPassword(e: FormEvent) {
    e.preventDefault();
    setPasswordMessage(null);
    if (newPassword !== confirmPassword) {
      setPasswordMessage({ kind: "error", text: "New passwords do not match." });
      return;
    }
    passwordMutation.mutate({ current_password: currentPassword, new_password: newPassword });
  }

  // An external login owns credentials and the change-password endpoint is not mounted.
  const canChangePassword = clientConfig?.password_auth_enabled !== false;

  return (
    <div className="space-y-6">
      <PageHeader title="Account" />

      <Card className="max-w-2xl">
        <CardHeader>
          <CardTitle className="text-base">Profile</CardTitle>
          <CardDescription>Your name and email on this account.</CardDescription>
        </CardHeader>
        <CardContent>
          <form className="space-y-4" onSubmit={onSubmitName}>
            <div className="space-y-2">
              <Label htmlFor="account-name">Name</Label>
              <Input
                id="account-name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                required
              />
            </div>
            <ReadonlyField
              id="account-email"
              label="Email"
              value={user.email}
              hint="Contact an admin to change the email on your account."
            />
            <StatusMessage message={nameMessage} />
            <Button type="submit" disabled={nameMutation.isPending || name.trim() === user.name}>
              {nameMutation.isPending ? "Saving…" : "Save name"}
            </Button>
          </form>
        </CardContent>
      </Card>

      {canChangePassword && (
        <Card className="max-w-2xl">
          <CardHeader>
            <CardTitle className="text-base">Password</CardTitle>
            <CardDescription>Change the password you use to sign in.</CardDescription>
          </CardHeader>
          <CardContent>
            <form className="space-y-4" onSubmit={onSubmitPassword}>
              <div className="space-y-2">
                <Label htmlFor="account-current-password">Current password</Label>
                <Input
                  id="account-current-password"
                  type="password"
                  autoComplete="current-password"
                  value={currentPassword}
                  onChange={(e) => setCurrentPassword(e.target.value)}
                  required
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="account-new-password">New password</Label>
                <Input
                  id="account-new-password"
                  type="password"
                  autoComplete="new-password"
                  value={newPassword}
                  onChange={(e) => setNewPassword(e.target.value)}
                  required
                  minLength={8}
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="account-confirm-password">Confirm new password</Label>
                <Input
                  id="account-confirm-password"
                  type="password"
                  autoComplete="new-password"
                  value={confirmPassword}
                  onChange={(e) => setConfirmPassword(e.target.value)}
                  required
                />
              </div>
              <StatusMessage message={passwordMessage} />
              <Button type="submit" disabled={passwordMutation.isPending}>
                {passwordMutation.isPending ? "Saving…" : "Change password"}
              </Button>
            </form>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
