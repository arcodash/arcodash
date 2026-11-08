import { useState } from "react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import { z } from "zod";
import { Link, Navigate, useLocation, useNavigate } from "react-router";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { AuthShell } from "@/components/AuthShell";
import { PageLoader } from "@/components/ui/loader";
import { api, apiErrorMessage } from "@/lib/api";
import { useAuth } from "@/lib/auth";
import { extensions } from "@/extensions";
import { useDocumentTitle } from "@/lib/useDocumentTitle";

const loginSchema = z.object({
  email: z.string().email("Enter a valid email address"),
  password: z.string().min(1, "Password is required"),
});

type LoginFormValues = z.infer<typeof loginSchema>;

const ldapLoginSchema = z.object({
  username: z.string().min(1, "Username is required"),
  password: z.string().min(1, "Password is required"),
});

type LdapLoginFormValues = z.infer<typeof ldapLoginSchema>;

// Only allow same-site, single-slash-rooted paths as a post-login destination; anything else
// (protocol-relative "//host", absolute URLs) falls back to the default in-app route.
export function safeDestination(path: string | undefined): string {
  if (path && path.startsWith("/") && !path.startsWith("//")) return path;
  return "/queries";
}

function SignInCard({ children }: { children: React.ReactNode }) {
  return (
    <AuthShell>
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle className="text-xl">Sign in</CardTitle>
          <CardDescription>Sign in to your arcodash account.</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">{children}</CardContent>
      </Card>
    </AuthShell>
  );
}

export function LoginPage() {
  useDocumentTitle("Sign in · arcodash");
  const { user, login, isLoading, clientConfig, refreshSession } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [serverError, setServerError] = useState<string | null>(null);
  const [mode, setMode] = useState<"password" | "ldap">("password");

  const {
    register,
    handleSubmit,
    formState: { errors, isSubmitting },
  } = useForm<LoginFormValues>({
    resolver: zodResolver(loginSchema),
    defaultValues: { email: "", password: "" },
  });

  const {
    register: registerLdap,
    handleSubmit: handleLdapSubmit,
    formState: { errors: ldapErrors, isSubmitting: ldapSubmitting },
  } = useForm<LdapLoginFormValues>({
    resolver: zodResolver(ldapLoginSchema),
    defaultValues: { username: "", password: "" },
  });

  if (isLoading) {
    return <PageLoader className="h-svh" />;
  }

  const destination = safeDestination((location.state as { from?: Location })?.from?.pathname);

  if (user) {
    return <Navigate to={destination} replace />;
  }

  if (!clientConfig) {
    return (
      <SignInCard>
        <p className="text-sm text-destructive">Couldn&apos;t reach the server. Please try again.</p>
        <Button type="button" className="w-full cursor-pointer" onClick={() => void refreshSession()}>
          Try again
        </Button>
      </SignInCard>
    );
  }

  async function submitWith(signIn: () => Promise<unknown>) {
    setServerError(null);
    try {
      await signIn();
      navigate(destination, { replace: true });
    } catch (err) {
      setServerError(apiErrorMessage(err, "Login failed"));
    }
  }

  const onSubmit = (values: LoginFormValues) => submitWith(() => login(values.email, values.password));
  const onLdapSubmit = (values: LdapLoginFormValues) =>
    submitWith(async () => {
      await api.ldapLogin(values.username, values.password);
      await refreshSession();
    });

  const serverErrorText = serverError && (
    <p className="text-sm text-destructive" aria-live="polite">
      {serverError}
    </p>
  );

  const slots = extensions.map((ext, i) => (ext.loginSlot ? <ext.loginSlot key={i} /> : null)).filter(Boolean);
  const showSsoButtons = slots.length > 0 || clientConfig.saml_enabled;

  return (
    <SignInCard>
      {clientConfig.password_auth_enabled === false ? (
        slots.length > 0 ? (
          <div className="space-y-2">{slots}</div>
        ) : (
          <p className="text-sm text-destructive">Sign-in is not configured. Contact your administrator.</p>
        )
      ) : (
        <>
          {showSsoButtons && (
            <div className="space-y-2">
              {slots}
              {clientConfig.saml_enabled && (
                <Button asChild variant="outline" className="w-full cursor-pointer">
                  <a href={api.samlLoginUrl()}>Sign in with SSO</a>
                </Button>
              )}
              <div className="relative py-2 text-center text-xs text-muted-foreground">
                <span className="bg-card px-2">or</span>
                <div className="absolute inset-x-0 top-1/2 -z-10 border-t" />
              </div>
            </div>
          )}

          {mode === "password" ? (
            <form className="space-y-4" onSubmit={handleSubmit(onSubmit)}>
              <div className="space-y-2">
                <Label htmlFor="email">Email</Label>
                <Input
                  id="email"
                  type="email"
                  autoComplete="email"
                  {...register("email")}
                />
                {errors.email && (
                  <p className="text-sm text-destructive">{errors.email.message}</p>
                )}
              </div>
              <div className="space-y-2">
                <Label htmlFor="password">Password</Label>
                <Input
                  id="password"
                  type="password"
                  autoComplete="current-password"
                  {...register("password")}
                />
                {errors.password && (
                  <p className="text-sm text-destructive">{errors.password.message}</p>
                )}
                <div className="text-right">
                  <Link
                    to="/forgot_password"
                    className="text-sm text-muted-foreground underline-offset-4 hover:underline"
                  >
                    Forgot password?
                  </Link>
                </div>
              </div>
              {serverErrorText}
              <Button type="submit" className="w-full" disabled={isSubmitting}>
                {isSubmitting ? "Signing in…" : "Sign in"}
              </Button>
            </form>
          ) : (
            <form className="space-y-4" onSubmit={handleLdapSubmit(onLdapSubmit)}>
              <div className="space-y-2">
                <Label htmlFor="ldap-username">Username</Label>
                <Input
                  id="ldap-username"
                  autoComplete="username"
                  {...registerLdap("username")}
                />
                {ldapErrors.username && (
                  <p className="text-sm text-destructive">{ldapErrors.username.message}</p>
                )}
              </div>
              <div className="space-y-2">
                <Label htmlFor="ldap-password">Password</Label>
                <Input
                  id="ldap-password"
                  type="password"
                  autoComplete="current-password"
                  {...registerLdap("password")}
                />
                {ldapErrors.password && (
                  <p className="text-sm text-destructive">{ldapErrors.password.message}</p>
                )}
              </div>
              {serverErrorText}
              <Button type="submit" className="w-full" disabled={ldapSubmitting}>
                {ldapSubmitting ? "Signing in…" : "Sign in"}
              </Button>
            </form>
          )}

          {clientConfig.ldap_enabled && (
            <button
              type="button"
              className="w-full cursor-pointer text-center text-sm text-muted-foreground underline-offset-4 hover:underline"
              onClick={() => {
                setServerError(null);
                setMode(mode === "password" ? "ldap" : "password");
              }}
            >
              {mode === "password" ? "Sign in with LDAP instead" : "Sign in with email and password instead"}
            </button>
          )}
        </>
      )}
    </SignInCard>
  );
}
