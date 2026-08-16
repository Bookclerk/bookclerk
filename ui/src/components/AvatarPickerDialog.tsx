import { useId, useRef, useState, type ReactNode } from "react";
import { Camera, Check, ImageOff } from "lucide-react";
import { UserAvatar } from "@/components/UserAvatar";
import { Button } from "@/components/ui/button";
import {
  gravatarUrl,
  resolveAvatar,
  selectedSourceKey,
  ssoProviderLabel,
  type AvatarFields,
} from "@/lib/avatar";
import { cn } from "@/lib/utils";

/**
 * Chooses among monogram, Gravatar, SSO pictures, and a manual upload.
 */
export function AvatarPickerDialog({
  open,
  onOpenChange,
  userId,
  label,
  fields,
  cacheKey,
  busy,
  onSelectSource,
  onUpload,
  onRemoveUpload,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  userId: number;
  label: string;
  fields: AvatarFields;
  cacheKey?: number;
  busy?: boolean;
  onSelectSource: (source: string) => void | Promise<void>;
  onUpload: (file: File) => void | Promise<void>;
  onRemoveUpload?: () => void | Promise<void>;
}) {
  const titleId = useId();
  const fileRef = useRef<HTMLInputElement>(null);
  const selected = selectedSourceKey(resolveAvatar(fields));
  const pictures = fields.sso_pictures ?? [];

  if (!open) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-scrim px-4 py-10 sm:items-center"
      data-avatar-picker=""
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !busy) onOpenChange(false);
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="w-full max-w-md rounded-lg border border-ink/10 bg-paper p-5 shadow-xl outline-none"
      >
        <div className="mb-4">
          <h2 id={titleId} className="font-display text-xl font-semibold text-ink">
            Profile picture
          </h2>
          <p className="mt-1 text-sm text-ink/55">
            Uploaded pictures come first. Otherwise the last sign-in provider
            picture is used, then Gravatar when an email is set.
          </p>
        </div>
        <div className="flex flex-wrap gap-3">
          <ChoiceButton
            selected={selected === "monogram"}
            disabled={busy}
            label="Monogram"
            onClick={() => void onSelectSource("monogram")}
          >
            <UserAvatar
              label={label}
              avatarSource="monogram"
              className="h-16 w-16 text-xl"
            />
          </ChoiceButton>
          {fields.gravatar_hash ? (
            <ChoiceButton
              selected={selected === "gravatar"}
              disabled={busy}
              label="Gravatar"
              onClick={() => void onSelectSource("gravatar")}
            >
              <RemotePreview src={gravatarUrl(fields.gravatar_hash, 128)} />
            </ChoiceButton>
          ) : null}
          {pictures.map((pic) => (
            <ChoiceButton
              key={pic.identity_id}
              selected={selected === `sso:${pic.identity_id}`}
              disabled={busy}
              label={ssoProviderLabel(pic.provider)}
              onClick={() => void onSelectSource(`sso:${pic.identity_id}`)}
            >
              <RemotePreview src={pic.picture_url} />
            </ChoiceButton>
          ))}
          <ChoiceButton
            selected={selected === "upload"}
            disabled={busy}
            label={fields.has_avatar ? "Uploaded" : "Upload"}
            onClick={() => {
              if (fields.has_avatar) {
                void onSelectSource("upload");
                return;
              }
              fileRef.current?.click();
            }}
          >
            {fields.has_avatar ? (
              <UserAvatar
                userId={userId}
                label={label}
                hasAvatar
                avatarSource="upload"
                cacheKey={cacheKey}
                className="h-16 w-16 text-xl"
              />
            ) : (
              <span className="flex h-16 w-16 items-center justify-center rounded-full bg-ink/10 text-ink">
                <Camera className="h-6 w-6" aria-hidden />
              </span>
            )}
          </ChoiceButton>
        </div>
        <input
          ref={fileRef}
          type="file"
          accept="image/jpeg,image/png,image/webp"
          className="sr-only"
          onChange={(e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (file) void onUpload(file);
          }}
        />
        <div className="mt-5 flex flex-wrap items-center justify-between gap-2">
          {fields.has_avatar && onRemoveUpload ? (
            <button
              type="button"
              className="text-xs font-medium text-ink/55 hover:text-ink disabled:opacity-50"
              disabled={busy}
              onClick={() => void onRemoveUpload()}
            >
              Remove uploaded picture
            </button>
          ) : (
            <span />
          )}
          <Button
            type="button"
            variant="ghost"
            disabled={busy}
            onClick={() => onOpenChange(false)}
          >
            Close
          </Button>
        </div>
      </div>
    </div>
  );
}

function ChoiceButton({
  selected,
  disabled,
  label,
  onClick,
  children,
}: {
  selected: boolean;
  disabled?: boolean;
  label: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className={cn(
        "relative flex w-20 flex-col items-center gap-1.5 rounded-md p-1 text-center text-xs font-medium text-ink/70 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal disabled:opacity-50",
        selected ? "text-ink" : "hover:bg-ink/5",
      )}
    >
      <span
        className={cn(
          "rounded-full",
          selected ? "ring-2 ring-teal ring-offset-2 ring-offset-paper" : "",
        )}
      >
        {children}
      </span>
      {selected ? (
        <span className="absolute right-1 top-1 flex h-4 w-4 items-center justify-center rounded-full bg-teal text-paper">
          <Check className="h-3 w-3" aria-hidden />
        </span>
      ) : null}
      <span>{label}</span>
    </button>
  );
}

function RemotePreview({ src }: { src: string }) {
  const [broken, setBroken] = useState(false);
  if (broken) {
    return (
      <span className="flex h-16 w-16 items-center justify-center rounded-full bg-ink/10 text-ink/40">
        <ImageOff className="h-6 w-6" aria-hidden />
      </span>
    );
  }
  return (
    <img
      src={src}
      alt=""
      referrerPolicy="no-referrer"
      className="h-16 w-16 rounded-full object-cover"
      onError={() => setBroken(true)}
    />
  );
}
