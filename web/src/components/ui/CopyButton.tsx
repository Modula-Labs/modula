import { useState } from "react";
import { Check, Copy } from "./icons";

export function CopyButton({ value, label = "Copy" }: { value: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      aria-label={label}
      onClick={async () => {
        await navigator.clipboard.writeText(value);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      }}
      className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground [&_svg]:size-4"
    >
      {copied ? <Check /> : <Copy />}
    </button>
  );
}
