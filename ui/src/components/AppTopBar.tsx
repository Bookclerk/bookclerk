import { useEffect, useId, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { LogOut, Menu, Settings2, X } from "lucide-react";
import { AppNav, type AppNavProps } from "@/components/AppNav";
import { BookclerkLogo } from "@/components/BookclerkLogo";
import { usePreferences } from "@/components/usePreferences";
import { navLinksForRole } from "@/lib/nav";
import { Button } from "@/components/ui/button";
import { pathForView } from "@/lib/routes";
import { cn } from "@/lib/utils";

/**
 * Shared authenticated page header (nav, actions, sign-out).
 *
 * @param props - Nav props, sign-out handler, and optional action slot.
 */
export function AppTopBar({
  nav,
  onSignOut,
  actions,
}: {
  nav: AppNavProps;
  onSignOut: () => void | Promise<void>;
  /** Kept visible on all widths (Refresh, Scan, Acquire, …). */
  actions?: ReactNode;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuId = useId();
  const { openPreferences, preferencesOpen } = usePreferences();
  const links = navLinksForRole(nav.role);

  useEffect(() => {
    if (!menuOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMenuOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [menuOpen]);

  useEffect(() => {
    if (!menuOpen) return;
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prev;
    };
  }, [menuOpen]);

  useEffect(() => {
    setMenuOpen(false);
  }, [nav.view]);

  function closeMenu() {
    setMenuOpen(false);
  }

  return (
    <>
      <div className="flex items-center justify-between gap-2 sm:gap-3">
        <div className="flex min-w-0 items-center gap-3 sm:gap-5">
          <a
            href="/"
            aria-label="Go to start page"
            className="inline-flex shrink-0 rounded-md outline-none focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal"
            onClick={(e) => {
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
              nav.onNavigate(nav.defaultView);
            }}
          >
            <BookclerkLogo className="h-8 w-auto sm:h-9" />
          </a>
          <div className="hidden min-w-0 lg:block">
            <AppNav {...nav} />
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1.5 sm:gap-2">
          {actions}
          <div className="hidden items-center gap-2 lg:flex">
            <Button
              variant="ghost"
              onClick={openPreferences}
              aria-label="Preferences"
              aria-haspopup="dialog"
              aria-expanded={preferencesOpen}
            >
              <Settings2 className="h-4 w-4" />
            </Button>
            <Button
              variant="ghost"
              onClick={() => void onSignOut()}
              aria-label="Sign out"
            >
              <LogOut className="h-4 w-4" />
            </Button>
          </div>
          <Button
            variant="ghost"
            className="lg:hidden"
            onClick={() => setMenuOpen((o) => !o)}
            aria-label={menuOpen ? "Close menu" : "Open menu"}
            aria-expanded={menuOpen}
            aria-controls={menuId}
          >
            {menuOpen ? <X className="h-5 w-5" /> : <Menu className="h-5 w-5" />}
          </Button>
        </div>
      </div>

      {menuOpen
        ? createPortal(
            <div className="fixed inset-0 z-[60] lg:hidden" role="presentation">
              <button
                type="button"
                className="absolute inset-0 bg-scrim backdrop-blur-[2px]"
                aria-label="Dismiss menu"
                onClick={closeMenu}
              />
              <div
                id={menuId}
                role="dialog"
                aria-modal="true"
                aria-label="Navigation menu"
                className={cn(
                  "absolute right-0 top-0 flex h-full w-[min(20rem,88vw)] flex-col",
                  "animate-[detailIn_200ms_ease-out] border-l border-ink/10 bg-paper shadow-xl",
                )}
              >
                <div className="flex items-center justify-between border-b border-ink/10 px-4 py-3">
                  <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                    Menu
                  </p>
                  <Button variant="ghost" onClick={closeMenu} aria-label="Close menu">
                    <X className="h-5 w-5" />
                  </Button>
                </div>
                <nav className="flex flex-1 flex-col gap-0.5 overflow-y-auto p-3">
                  {links.map((link) => (
                    <a
                      key={link.id}
                      href={pathForView(link.id)}
                      onClick={(e) => {
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
                        closeMenu();
                        nav.onNavigate(link.id);
                      }}
                      className={cn(
                        "rounded-md px-3 py-2.5 text-sm transition-colors",
                        nav.view === link.id
                          ? "bg-teal/15 font-semibold text-ink"
                          : "text-ink/70 hover:bg-ink/5 hover:text-ink",
                      )}
                      aria-current={nav.view === link.id ? "page" : undefined}
                    >
                      {link.label}
                    </a>
                  ))}
                </nav>
                <div className="space-y-1 border-t border-ink/10 p-3">
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 rounded-md px-3 py-2.5 text-left text-sm text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink"
                    onClick={() => {
                      closeMenu();
                      openPreferences();
                    }}
                  >
                    <Settings2 className="h-4 w-4 shrink-0" />
                    Preferences
                  </button>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 rounded-md px-3 py-2.5 text-left text-sm text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink"
                    onClick={() => {
                      closeMenu();
                      void onSignOut();
                    }}
                  >
                    <LogOut className="h-4 w-4 shrink-0" />
                    Sign out
                  </button>
                </div>
              </div>
            </div>,
            document.body,
          )
        : null}
    </>
  );
}
