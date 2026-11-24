import { createFileRoute } from "@tanstack/react-router";
import { Button } from "@/components/ui/button";
import { env } from "@/env";

export const Route = createFileRoute("/")({
  component: Home,
});

function Home() {
  return (
    <div className="flex flex-col items-start gap-4">
      <div className="flex items-center gap-3">
        <img src="/logo.png" alt="" className="size-10 rounded-full" />
        <h1 className="text-2xl font-semibold">overseer ({env.MODE})</h1>
      </div>
      <Button>New session</Button>
    </div>
  );
}
