import { Button } from "@/components/ui/button";
import { HugeiconsIcon } from "@hugeicons/react";
import { Home01Icon } from "@hugeicons/core-free-icons";
import "./App.css";

function App() {
  return (
    <main className="container flex min-h-screen flex-col items-center justify-center gap-4">
      <h1 className="text-4xl font-bold">DjinnOS Desktop</h1>
      <p className="text-muted-foreground">
        shadcn/ui with Base UI primitives, Geist font, and Huge Icons
      </p>
      <div className="flex gap-4">
        <Button>Default Button</Button>
        <Button variant="secondary">Secondary</Button>
        <Button variant="outline">Outline</Button>
        <Button variant="ghost">Ghost</Button>
      </div>
      <div className="flex gap-4">
        <Button size="sm">Small</Button>
        <Button size="default">Default</Button>
        <Button size="lg">Large</Button>
      </div>
      <div className="flex gap-4 items-center">
        <HugeiconsIcon icon={Home01Icon} size={24} className="text-primary" />
        <span className="text-sm text-muted-foreground">Huge Icons</span>
      </div>
    </main>
  );
}

export default App;
