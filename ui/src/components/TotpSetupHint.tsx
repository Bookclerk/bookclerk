import { useState } from "react";
import { Button } from "@/components/ui/button";

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/**
 * QR, copyable setup key, and otpauth link for TOTP enrollment.
 *
 * @param props - Pending secret, otpauth URI, and QR SVG from enroll-begin.
 */
export function TotpSetupHint({
  secret,
  otpauthUrl,
  qrSvg,
}: {
  secret: string;
  otpauthUrl: string;
  qrSvg: string;
}) {
  const [copied, setCopied] = useState(false);

  async function onCopy() {
    const ok = await copyText(secret);
    if (!ok) return;
    setCopied(true);
    window.setTimeout(() => setCopied(false), 2000);
  }

  return (
    <>
      <img
        alt="Authenticator QR code"
        className="mx-auto h-40 w-40 rounded-md border border-ink/10 bg-white"
        src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(qrSvg)}`}
      />
      <div className="flex items-center gap-2 rounded-md border border-ink/15 bg-card-strong px-3 py-2">
        <p className="min-w-0 flex-1 break-all font-mono text-xs text-ink/80">{secret}</p>
        <Button
          type="button"
          variant="secondary"
          className="h-8 shrink-0"
          onClick={() => void onCopy()}
        >
          {copied ? "Copied" : "Copy"}
        </Button>
      </div>
      <a
        className="text-sm text-teal underline-offset-2 hover:underline"
        href={otpauthUrl}
      >
        Open in authenticator app
      </a>
    </>
  );
}
