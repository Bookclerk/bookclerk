import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";

export function ErrorStatePage({
  title,
  message,
  retryLabel = "Try again",
  onRetry,
}: {
  title: string;
  message: string;
  retryLabel?: string;
  onRetry?: () => void;
}) {
  return (
    <section className="rounded-lg border border-brick/25 bg-brick/5 p-6">
      <div className="flex items-start gap-3">
        <AlertTriangle className="mt-0.5 h-5 w-5 text-brick" />
        <div className="space-y-2">
          <h2 className="text-lg font-semibold text-ink">{title}</h2>
          <p className="text-sm text-ink/70">{message}</p>
          {onRetry ? (
            <Button variant="secondary" onClick={onRetry}>
              {retryLabel}
            </Button>
          ) : null}
        </div>
      </div>
    </section>
  );
}
