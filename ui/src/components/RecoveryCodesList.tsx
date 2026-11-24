import { useState } from "react";
import { Check, Copy, Download } from "lucide-react";
import { Button } from "@/components/ui/button";

/**
 * A set of recovery codes, shown once: the codes, ways to keep them, and a
 * button to say they are saved.
 *
 * @param props.codes - The ten codes, each `XXXX-XXXX-XXXX-XXXX`.
 * @param props.onDone - Called when the patron says the codes are saved.
 * @param props.doneLabel - The label of that button.
 */
export function RecoveryCodesList({
  codes,
  onDone,
  doneLabel = "I saved my codes",
}: {
  codes: string[];
  onDone: () => void;
  doneLabel?: string;
}) {
  const [copied, setCopied] = useState(false);
  const text = `Bookclerk recovery codes\nEach code works once.\n\n${codes.join("\n")}\n`;

  async function onCopy() {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }

  function onDownload() {
    const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "bookclerk-recovery-codes.txt";
    link.click();
    URL.revokeObjectURL(url);
  }

  return (
    <div className="flex flex-col gap-3">
      <p className="text-sm text-ink/70">
        Keep these recovery codes somewhere safe, such as a password manager. If you lose your
        authenticator app or passkeys, one of them lets you reset your password from a reset email.
        Each code works once, and they are not shown again.
      </p>
      <ul
        aria-label="Recovery codes"
        className="grid grid-cols-2 gap-x-4 gap-y-1.5 rounded-md border border-ink/10 bg-card-mid p-3 font-mono text-sm text-ink"
      >
        {codes.map((code) => (
          <li key={code}>{code}</li>
        ))}
      </ul>
      <div className="flex flex-wrap gap-2">
        <Button type="button" variant="secondary" onClick={() => void onCopy()}>
          {copied ? (
            <Check className="h-4 w-4" aria-hidden />
          ) : (
            <Copy className="h-4 w-4" aria-hidden />
          )}
          {copied ? "Copied" : "Copy"}
        </Button>
        <Button type="button" variant="secondary" onClick={onDownload}>
          <Download className="h-4 w-4" aria-hidden />
          Download
        </Button>
      </div>
      <Button type="button" onClick={onDone}>
        {doneLabel}
      </Button>
    </div>
  );
}
