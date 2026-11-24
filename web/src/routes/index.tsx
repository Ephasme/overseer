import { createFileRoute } from "@tanstack/react-router";
import { Button } from "@/components/ui/button";
import { env } from "@/env";

export const Route = createFileRoute("/")({
  component: Home,
});

function Home() {
  return (
    <div className="flex flex-col items-start gap-4">
      <h1 className="text-2xl font-semibold">overseer ({env.MODE})</h1>
      <Button>New session</Button>
    </div>
  );
}
