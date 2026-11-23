import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CoverThumb } from "@/components/CoverThumb";
import { StoreLogo } from "@/components/StoreLogo";
import { storeLabel } from "@/lib/catalogTitle";
import { clerkRequestFailure, requestFromLibrary } from "@/lib/clerk";
import {
  windowEntryFormats,
  windowEntryIsYours,
  windowEntryLibraries,
  windowEntryRequest,
  type WindowEntryView,
  type WindowRequest,
} from "@/lib/join";

/** Where a request to a library stands. */
type RequestState =
  { kind: "idle" } | { kind: "sending" } | { kind: "sent" } | { kind: "failed"; message: string };

/**
 * Asks the library for a copy of a book it has none of: the library
 * fetches it from a branch, or downloads it on its own store account, as
 * for a request from the person's browser there.
 */
function RequestButton({ ask }: { ask: WindowRequest }) {
  const [state, setState] = useState<RequestState>({ kind: "idle" });
  const send = async () => {
    setState({ kind: "sending" });
    try {
      await requestFromLibrary(ask.libraryId, ask.workId, ask.from);
      setState({ kind: "sent" });
    } catch (err) {
      setState({ kind: "failed", message: clerkRequestFailure(err) });
    }
  };
  if (state.kind === "sent") {
    return (
      <p className="mt-1 text-xs text-ink/70" role="status">
        {ask.from === "library"
          ? `Requested. ${ask.host} is downloading it; it shows here once it's there.`
          : `Requested. It shows with your requests on ${ask.host}.`}
      </p>
    );
  }
  return (
    <div className="mt-1 flex flex-wrap items-center gap-2">
      <Button
        variant="secondary"
        className="h-7 px-2 text-xs"
        disabled={state.kind === "sending"}
        onClick={() => void send()}
      >
        {state.kind === "sending" ? "Requesting…" : "Request"}
      </Button>
      {state.kind === "failed" ? (
        <span className="text-xs text-ink/70" role="alert">
          {state.message}
        </span>
      ) : null}
    </div>
  );
}

/**
 * One row of the Library list for a book only other libraries hold, laid
 * out like a work's row: where it is (in place of an acquire state),
 * marked Yours when it is on the person's own store accounts there, with
 * a Request button when no library holding it has a copy yet (it is on a
 * store account there, or only its branches hold it).
 */
export function RemoteWorkRow({ entry }: { entry: WindowEntryView }) {
  const first = entry.editions[0];
  if (!first) return null;
  const ask = windowEntryRequest(entry);
  const series = first.series
    ? `${first.series}${first.series_index ? ` #${first.series_index}` : ""}`
    : null;
  const stores = [...new Set(entry.editions.map((e) => e.store))];
  const where =
    ask?.from === "branch"
      ? `On a branch of ${ask.host}`
      : `In ${windowEntryLibraries(entry).join(", ")}`;
  return (
    <div className="grid grid-cols-[56px_1fr] items-center gap-3 border-b border-ink/10 px-3 py-2.5 sm:grid-cols-[64px_1fr] sm:gap-4 sm:px-4">
      <div className="relative h-14 w-14 overflow-hidden rounded-md bg-fold shadow-sm sm:h-16 sm:w-16">
        <CoverThumb className="h-full w-full rounded-none bg-transparent shadow-none" />
      </div>
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="truncate font-display text-base font-semibold leading-tight text-ink sm:text-lg">
            {first.title}
          </h2>
          <Badge className="bg-parchment normal-case tracking-normal text-ink">{where}</Badge>
          {windowEntryIsYours(entry) ? <Badge className="bg-ink text-paper">Yours</Badge> : null}
          {windowEntryFormats(entry).map((format) => (
            <Badge key={format} className="bg-ink/8 text-ink/70">
              {format === "pdf" ? "PDF" : "Audio"}
            </Badge>
          ))}
        </div>
        {first.authors ? (
          <p className="mt-0.5 truncate text-sm text-ink/70">
            {[first.authors, first.narrators ? `narr. ${first.narrators}` : null]
              .filter(Boolean)
              .join(" · ")}
          </p>
        ) : null}
        <p className="mt-0.5 flex flex-wrap items-center gap-1.5 text-xs text-ink/70">
          {stores.map((store) => (
            <span key={store} className="inline-flex items-center gap-1">
              <StoreLogo source={store} className="h-4 w-4" />
              {storeLabel(store)}
            </span>
          ))}
          {series ? <span>· {series}</span> : null}
        </p>
        {ask ? <RequestButton key={`${ask.libraryId}/${ask.workId}`} ask={ask} /> : null}
      </div>
    </div>
  );
}

/**
 * The line under a work of this library that other libraries hold too:
 * which ones, and Yours when it is on the person's own store accounts in
 * one of them.
 */
export function ElsewhereNote({ entries }: { entries: WindowEntryView[] }) {
  const hosts = [...new Set(entries.flatMap(windowEntryLibraries))];
  if (hosts.length === 0) return null;
  return (
    <p className="mt-0.5 flex flex-wrap items-center gap-1.5 text-xs text-ink/70">
      <span>Also in {hosts.join(", ")}</span>
      {entries.some(windowEntryIsYours) ? (
        <Badge className="bg-ink text-paper">Yours there</Badge>
      ) : null}
    </p>
  );
}
