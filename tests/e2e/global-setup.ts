// The backend bootstraps the admin only at startup; a reused backend or a modified test DB can
// leave it missing, which surfaces as 401 at every login.
export default async function globalSetup() {
  const backend = process.env.BACKEND_URL ?? "http://localhost:8080";
  const email = "admin@example.com";
  const password = "adminpassword";
  const res = await fetch(`${backend}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  if (res.ok) return;
  throw new Error(
    `e2e admin login failed (${res.status}) for ${email} at ${backend}.\n` +
      `The backend creates this admin only at startup; a reused backend may be running ` +
      `against a test DB that no longer has it.\n` +
      `Stop any other backend on :8080 and rerun so a fresh backend re-bootstraps the admin.`,
  );
}
