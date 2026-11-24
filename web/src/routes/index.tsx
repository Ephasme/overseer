import { createFileRoute } from "@tanstack/react-router";
import { env } from "@/env";

export const Route = createFileRoute("/")({
  component: Home,
});

function Home() {
  return <h1 className="text-2xl font-semibold">overseer ({env.MODE})</h1>;
}
