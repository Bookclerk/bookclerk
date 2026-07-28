import { cn } from "@/lib/utils";
import type { AppView } from "@/lib/api";

export interface AppNavProps {
  view: AppView;
  onNavigate: (view: AppView) => void;
}

const LINKS: { id: AppView; label: string }[] = [
  { id: "discover", label: "Discover" },
  { id: "wishlist", label: "Wishlist" },
  { id: "library", label: "Library" },
  { id: "accounts", label: "Accounts" },
];

export function AppNav({ view, onNavigate }: AppNavProps) {
  return (
    <nav className="flex flex-wrap items-center gap-1 text-sm sm:gap-2">
      {LINKS.map((link) => (
        <button
          key={link.id}
          type="button"
          onClick={() => onNavigate(link.id)}
          className={cn(
            "rounded-md px-2 py-1 transition-colors",
            view === link.id
              ? "font-semibold text-ink"
              : "text-ink/55 hover:bg-ink/5 hover:text-ink",
          )}
          aria-current={view === link.id ? "page" : undefined}
        >
          {link.label}
        </button>
      ))}
    </nav>
  );
}
