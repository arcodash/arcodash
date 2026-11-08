import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "react-router";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { AuthShell } from "@/components/AuthShell";
import { PasswordField, PASSWORD_MIN_LENGTH, PASSWORD_TOO_SHORT_MESSAGE } from "@/components/PasswordField";
import { ApiError, api, apiErrorMessage } from "@/lib/api";
import { purgeNonSessionQueries, useAuth } from "@/lib/auth";
import { useScrubTokenParam } from "@/hooks/useScrubTokenParam";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

function TokenProblemCard({
  headingRef,
  title,
  children,
}: {
  headingRef: React.Ref<HTMLHeadingElement>;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <AuthShell>
      <Card className="w-full max-w-sm">
        <CardHeader>
          <h1 ref={headingRef} tabIndex={-1} className="text-xl leading-none font-semibold outline-none">
            {title}
          </h1>
        </CardHeader>
        <CardContent className="space-y-4">{children}</CardContent>
      </Card>
    </AuthShell>
  );
}

export function InviteAcceptPage() {
  useDocumentTitle("Accept invitation · arcodash");
  const token = useScrubTokenParam("token");
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { refreshSession } = useAuth();

  const [inviteEmail, setInviteEmail] = useState("");
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [passwordError, setPasswordError] = useState<string | null>(null);
  const [serverError, setServerError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [verifying, setVerifying] = useState(true);
  // Every invalid-token condition (missing, used, unrecognized; at verify or submit) collapses to
  // one terminal state: this link doesn't work anymore. Expired gets its own state since the fix
  // (ask for a new invitation) differs.
  const [tokenInvalid, setTokenInvalid] = useState(false);
  const [tokenExpired, setTokenExpired] = useState(false);
  // A verify call that got no real server answer (network failure, 5xx, timeout), unlike
  // `tokenInvalid`; retryable since the invite may still be valid.
  const [verifyFailed, setVerifyFailed] = useState(false);

  const headingRef = useRef<HTMLHeadingElement>(null);

  // The backend answers a bad token with 400 (invalid) or 400 + `invitation_expired` (expired).
  // Anything else (no `ApiError`, non-400) means no real answer, not a bad token.
  function classifyTokenError(err: unknown) {
    if (err instanceof ApiError) {
      if (err.errorCode === "invitation_expired") {
        setTokenExpired(true);
        return;
      }
      if (err.status === 400) {
        setTokenInvalid(true);
        return;
      }
    }
    setVerifyFailed(true);
  }

  function runVerify() {
    if (!token) {
      setVerifying(false);
      setTokenInvalid(true);
      return;
    }
    setVerifying(true);
    setVerifyFailed(false);
    api
      .verifyInvitation(token)
      .then((data) => { setInviteEmail(data.email); setName(data.name); })
      .catch(classifyTokenError)
      .finally(() => setVerifying(false));
  }

  useEffect(() => {
    runVerify();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- runVerify closes over `token`, the only thing this should re-run for
  }, [token]);

  useEffect(() => {
    if (tokenInvalid || tokenExpired || verifyFailed) headingRef.current?.focus();
  }, [tokenInvalid, tokenExpired, verifyFailed]);

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (password.length < PASSWORD_MIN_LENGTH) {
      setPasswordError(PASSWORD_TOO_SHORT_MESSAGE);
      return;
    }
    setServerError(null);
    setSubmitting(true);
    try {
      await api.acceptInvitation({ token, name, password });
      purgeNonSessionQueries(queryClient);
      await refreshSession();
      // "/" resolves to the first nav route this user can reach, never a hardcoded page a limited
      // invitee couldn't use.
      navigate("/", { replace: true });
    } catch (err) {
      if (err instanceof ApiError && err.status === 400) {
        classifyTokenError(err);
        return;
      }
      setServerError(apiErrorMessage(err, "Failed to accept invitation"));
      setSubmitting(false);
    }
  }

  if (verifyFailed) {
    return (
      <TokenProblemCard headingRef={headingRef} title="Couldn't verify this invitation">
        <p className="text-sm text-muted-foreground">
          Something went wrong reaching the server. Your invitation link may still be valid.
        </p>
        <Button className="w-full" onClick={runVerify}>
          Try again
        </Button>
      </TokenProblemCard>
    );
  }

  if (tokenExpired || tokenInvalid) {
    return (
      <TokenProblemCard
        headingRef={headingRef}
        title={tokenExpired ? "This invitation link has expired" : "This invitation link isn't valid"}
      >
        <p className="text-sm text-muted-foreground">
          {tokenExpired
            ? "Invitation links are only valid for a limited time, and this one is no longer active."
            : "It may already have been used, or the link may be incomplete."}
        </p>
        <p className="text-sm text-muted-foreground">Ask whoever invited you to send a new invitation.</p>
        <Button asChild className="w-full">
          <Link to="/login">Go to sign in</Link>
        </Button>
      </TokenProblemCard>
    );
  }

  return (
    <AuthShell>
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle className="text-xl">Accept Invitation</CardTitle>
          {inviteEmail && <CardDescription>Setting up account for {inviteEmail}</CardDescription>}
        </CardHeader>
        <CardContent>
          {verifying && <p className="text-muted-foreground">Verifying…</p>}
          {!verifying && inviteEmail && (
            <form className="space-y-4" onSubmit={onSubmit}>
              <div className="space-y-2">
                <Label htmlFor="inv-name">Name</Label>
                <Input id="inv-name" value={name} onChange={(e) => setName(e.target.value)} required />
              </div>
              <PasswordField
                idPrefix="inv-pw"
                label="Password"
                value={password}
                onChange={setPassword}
                error={passwordError}
                onErrorClear={() => setPasswordError(null)}
                autoComplete="new-password"
              />
              {serverError && <p className="text-sm text-destructive">{serverError}</p>}
              <Button type="submit" className="w-full" disabled={submitting}>
                {submitting ? "Creating your account…" : "Create account"}
              </Button>
            </form>
          )}
        </CardContent>
      </Card>
    </AuthShell>
  );
}
