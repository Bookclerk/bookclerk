import { useCallback, useEffect, useState } from "react";
import { Headphones, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { joinErrorMessage } from "@/lib/join";
import {
  createPlayerPassword,
  deletePlayerPassword,
  listPlayerPasswords,
  type NewPlayerPassword,
  type PlayerPasswordView,
} from "@/lib/playerApps";

/**
 * A date as the list shows it.
 *
 * @param iso - An RFC 3339 time, if any.
 * @returns The local date and time, or "never".
 */
function when(iso: string | null | undefined): string {
  if (!iso) return "never";
  const date = new Date(iso);
  return Number.isNaN(date.getTime())
    ? iso
    : date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

/**
 * Account settings: passwords for audiobook player apps.
 *
 * An app written for an Audiobookshelf server signs in at this library's
 * address plus `/player` with the user's sign-in name and one of these
 * passwords (never the account password), or through the app's browser
 * sign-in. Each password is shown once.
 */
export function PlayerPasswordsSection() {
  const [passwords, setPasswords] = useState<PlayerPasswordView[]>([]);
  const [label, setLabel] = useState("");
  const [made, setMade] = useState<NewPlayerPassword | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setPasswords(await listPlayerPasswords());
    } catch (err) {
      setError(joinErrorMessage(err, "Could not read your player passwords."));
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    listPlayerPasswords()
      .then((list) => {
        if (!cancelled) setPasswords(list);
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(joinErrorMessage(err, "Could not read your player passwords."));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function create() {
    setBusy(true);
    setError(null);
    try {
      const fresh = await createPlayerPassword(label.trim() || "Player app");
      setMade(fresh);
      setLabel("");
      await refresh();
    } catch (err) {
      setError(joinErrorMessage(err, "Could not make a player password."));
    } finally {
      setBusy(false);
    }
  }

  async function remove(id: string) {
    setBusy(true);
    setError(null);
    try {
      await deletePlayerPassword(id);
      if (made?.password.id === id) setMade(null);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not remove the player password.");
    } finally {
      setBusy(false);
    }
  }

  const server = `${window.location.origin}/player`;

  return (
    <section className="space-y-3">
      <div className="space-y-1">
        <h2 className="flex items-center gap-2 text-lg font-semibold text-ink">
          <Headphones className="h-4 w-4 text-ink/50" />
          Player apps
        </h2>
        <p className="text-sm text-ink/55">
          Listen in apps made for Audiobookshelf servers. Use{" "}
          <span className="font-mono text-xs">{server}</span> as the server address, your sign-in
          name, and a player password from here. Your account password never works in an app.
        </p>
      </div>
      <form
        className="flex flex-wrap gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          void create();
        }}
      >
        <Input
          value={label}
          maxLength={64}
          placeholder="Name it after the device, such as Phone"
          aria-label="Player password name"
          className="max-w-xs"
          onChange={(event) => setLabel(event.target.value)}
        />
        <Button type="submit" disabled={busy}>
          Make a player password
        </Button>
      </form>
      {made ? (
        <div className="rounded-md border border-teal/40 bg-teal/5 p-3 text-sm" role="status">
          <p className="text-ink/75">
            The password for <strong>{made.password.label}</strong>. Copy it now: it will not be
            shown again.
          </p>
          <p className="mt-2 font-mono text-base tracking-wider text-ink">{made.secret}</p>
        </div>
      ) : null}
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      {passwords.length > 0 ? (
        <ul className="divide-y divide-ink/10 rounded-md border border-ink/10">
          {passwords.map((password) => (
            <li key={password.id} className="flex items-center justify-between gap-3 p-3">
              <div className="min-w-0">
                <p className="truncate text-sm font-medium text-ink">{password.label}</p>
                <p className="text-xs text-ink/50">
                  Made {when(password.created_at)} · Last used {when(password.last_used_at)}
                </p>
              </div>
              <Button
                type="button"
                variant="ghost"
                disabled={busy}
                aria-label={`Remove ${password.label}`}
                onClick={() => void remove(password.id)}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </li>
          ))}
        </ul>
      ) : null}
    </section>
  );
}
