import { createRootRoute, Outlet } from "@tanstack/react-router";

export const Route = createRootRoute({
  component: () => (
    <main className="min-h-svh p-6">
      <Outlet />
    </main>
  ),
});
