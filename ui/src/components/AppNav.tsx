import { cn } from "@/lib/utils";
import type { AppView, AuthRole } from "@/lib/api";
import { pathForView } from "@/lib/routes";

/**
 * Props for {@link AppNav}.
 */
export interface AppNavProps {
  /** Currently active app view. */
  view: AppView;
  /** In-app navigation callback (history is updated by the parent). */
  onNavigate: (view: AppView) => void;
  /** Effective session role (Settings is available to all signed-in roles). */
  role?: AuthRole;
}

const BASE_LINKS: { id: AppView; label: string }[] = [
  { id: "discover", label: "Discover" },
  { id: "wishlist", label: "Wishlist" },
  { id: "library", label: "Library" },
  { id: "accounts", label: "Accounts" },
];

/**
 * Whether Settings appears in primary nav for this role.
 *
 * All signed-in roles get Settings (Account with Profile / Security / Sessions).
 * User Management / Server / Plugins tabs are gated inside the page by role.
 */
export function showSettingsNav(_role?: AuthRole): boolean {
  return true;
}

/**
 * Primary nav links for an effective role (Settings always included).
 */
export function navLinksForRole(
  _role?: AuthRole,
): { id: AppView; label: string }[] {
  return [...BASE_LINKS, { id: "settings", label: "Settings" }];
}

/**
 * Primary in-app navigation links for authenticated views.
 *
 * @param props - Current view and navigate callback.
 */
export function AppNav({ view, onNavigate, role }: AppNavProps) {
  return (
    <nav className="flex flex-wrap items-center gap-1 text-sm sm:gap-2">
      {navLinksForRole(role).map((link) => (
        <a
          key={link.id}
          href={pathForView(link.id)}
          onClick={(e) => {
            // Keep in-app navigation instant; allow modified clicks to open a tab.
            if (
              e.defaultPrevented ||
              e.button !== 0 ||
              e.metaKey ||
              e.ctrlKey ||
              e.shiftKey ||
              e.altKey
            ) {
              return;
            }
            e.preventDefault();
            onNavigate(link.id);
          }}
          className={cn(
            "rounded-md px-2 py-1 transition-colors",
            view === link.id
              ? "font-semibold text-ink"
              : "text-ink/55 hover:bg-ink/5 hover:text-ink",
          )}
          aria-current={view === link.id ? "page" : undefined}
        >
          {link.label}
        </a>
      ))}
    </nav>
  );
}
