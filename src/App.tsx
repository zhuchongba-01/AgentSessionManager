import { SessionManagerPage } from "@/components/sessions/SessionManagerPage";

export default function App() {
  return (
    <main className="h-screen min-h-0 overflow-hidden bg-background text-foreground">
      <SessionManagerPage appId="all" />
    </main>
  );
}
