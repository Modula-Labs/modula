import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "./styles.css";
import "./lib/theme";

import { QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { ApiError } from "./lib/api";
import { makeRouter } from "./router";

const queryClient: QueryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: (n, e) => !(e instanceof ApiError && e.status < 500) && n < 2,
      refetchOnWindowFocus: false,
    },
  },
  queryCache: new QueryCache({
    // A session that expires mid-use sends the user back to sign in.
    onError: (e) => {
      if (e instanceof ApiError && e.status === 401 && !location.pathname.startsWith("/login")) {
        queryClient.clear();
        void router.navigate({ to: "/login", search: { expired: true } });
      }
    },
  }),
});
const router = makeRouter(queryClient);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
