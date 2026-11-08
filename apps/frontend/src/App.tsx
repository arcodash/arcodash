import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "react-router";
import { router } from "@/router";
import { AuthProvider } from "@/lib/auth";
import { queryClient } from "@/lib/sessionExpiry";

// The client and its 401 handler live in `lib/sessionExpiry.ts` so `lib/api.ts` can reach the
// handler too without an App↔api import cycle; re-exported here for convenience.
export { queryClient, handleExpiredSession } from "@/lib/sessionExpiry";

function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>
  );
}

export default App;
