import type { ButtonHTMLAttributes, SVGProps } from "react";
import googleMark from "@/assets/sso/google.svg";
import { Button } from "@/components/ui/button";
import { useResolvedDark } from "@/components/ThemeProvider";
import { cn } from "@/lib/utils";

const BRAND_NAMES: Record<string, string> = {
  google: "Google",
  github: "GitHub",
  apple: "Apple",
  discord: "Discord",
};

/** Official Sign in with Apple logo-only path (Apple Design Resources). */
const APPLE_PATH =
  "M28.2226562,20.3846154 C29.0546875,20.3846154 30.0976562,19.8048315 30.71875,19.0317864 C31.28125,18.3312142 31.6914062,17.352829 31.6914062,16.3744437 C31.6914062,16.2415766 31.6796875,16.1087095 31.65625,16 C30.7304687,16.0362365 29.6171875,16.640178 28.9492187,17.4494596 C28.421875,18.06548 27.9414062,19.0317864 27.9414062,20.0222505 C27.9414062,20.1671964 27.9648438,20.3121424 27.9765625,20.3604577 C28.0351562,20.3725366 28.1289062,20.3846154 28.2226562,20.3846154 Z M25.2929688,35 C26.4296875,35 26.9335938,34.214876 28.3515625,34.214876 C29.7929688,34.214876 30.109375,34.9758423 31.375,34.9758423 C32.6171875,34.9758423 33.4492188,33.792117 34.234375,32.6325493 C35.1132812,31.3038779 35.4765625,29.9993643 35.5,29.9389701 C35.4179688,29.9148125 33.0390625,28.9122695 33.0390625,26.0979021 C33.0390625,23.6579784 34.9140625,22.5588048 35.0195312,22.474253 C33.7773438,20.6382708 31.890625,20.5899555 31.375,20.5899555 C29.9804688,20.5899555 28.84375,21.4596313 28.1289062,21.4596313 C27.3554688,21.4596313 26.3359375,20.6382708 25.1289062,20.6382708 C22.8320312,20.6382708 20.5,22.5950413 20.5,26.2911634 C20.5,28.5861411 21.3671875,31.013986 22.4335938,32.5842339 C23.3476562,33.9129053 24.1445312,35 25.2929688,35 Z";

/** Official GitHub Invertocat (primer/octicons mark-github-24). */
const GITHUB_PATH =
  "M10.226 17.284c-2.965-.36-5.054-2.493-5.054-5.256 0-1.123.404-2.336 1.078-3.144-.292-.741-.247-2.314.09-2.965.898-.112 2.111.36 2.83 1.01.853-.269 1.752-.404 2.853-.404 1.1 0 1.999.135 2.807.382.696-.629 1.932-1.1 2.83-.988.315.606.36 2.179.067 2.942.72.854 1.101 2 1.101 3.167 0 2.763-2.089 4.852-5.098 5.234.763.494 1.28 1.572 1.28 2.807v2.336c0 .674.561 1.056 1.235.786 4.066-1.55 7.255-5.615 7.255-10.646C23.5 6.188 18.334 1 11.978 1 5.62 1 .5 6.188.5 12.545c0 4.986 3.167 9.12 7.435 10.669.606.225 1.19-.18 1.19-.786V20.63a2.9 2.9 0 0 1-1.078.224c-1.483 0-2.359-.808-2.987-2.313-.247-.607-.517-.966-1.034-1.033-.27-.023-.359-.135-.359-.27 0-.27.45-.471.898-.471.652 0 1.213.404 1.797 1.235.45.651.921.943 1.483.943.561 0 .92-.202 1.437-.719.382-.381.674-.718.944-.943";

/** Official Discord Clyde symbol (discord.com/branding). */
const DISCORD_PATH =
  "M37.1937 0C36.6265 1.0071 36.1172 2.04893 35.6541 3.11392C31.2553 2.45409 26.7754 2.45409 22.365 3.11392C21.9136 2.04893 21.3926 1.0071 20.8254 0C16.6928 0.70613 12.6644 1.94475 8.84436 3.69271C1.27372 14.9098 -0.775214 25.8374 0.243466 36.6146C4.67704 39.8906 9.6431 42.391 14.9333 43.9884C16.1256 42.391 17.179 40.6893 18.0819 38.9182C16.3687 38.2815 14.7133 37.4828 13.1274 36.5567C13.5442 36.2557 13.9493 35.9432 14.3429 35.6422C23.6384 40.0179 34.4039 40.0179 43.711 35.6422C44.1046 35.9663 44.5097 36.2789 44.9264 36.5567C43.3405 37.4943 41.6852 38.2815 39.9604 38.9298C40.8633 40.7009 41.9167 42.4025 43.109 44C48.3992 42.4025 53.3653 39.9137 57.7988 36.6377C59.0027 24.1358 55.7383 13.3007 49.1748 3.70429C45.3663 1.95633 41.3379 0.717706 37.2053 0.0231518L37.1937 0ZM19.3784 29.9816C16.5192 29.9816 14.1461 27.3886 14.1461 24.1821C14.1461 20.9755 16.4266 18.371 19.3669 18.371C22.3071 18.371 24.6455 20.9871 24.5992 24.1821C24.5529 27.377 22.2956 29.9816 19.3784 29.9816ZM38.6639 29.9816C35.7931 29.9816 33.4431 27.3886 33.4431 24.1821C33.4431 20.9755 35.7236 18.371 38.6639 18.371C41.6042 18.371 43.9309 20.9871 43.8846 24.1821C43.8383 27.377 41.581 29.9816 38.6639 29.9816Z";

const MONO_MARKS: Record<
  string,
  { viewBox: string; path: string; fillRule?: SVGProps<SVGPathElement>["fillRule"] }
> = {
  apple: {
    // Official SIWA canvas is padded (6 6 44 44). Crop to the glyph so the
    // 24px tile matches Discord / GitHub optical size.
    viewBox: "19.2 14.6 17.6 21.6",
    path: APPLE_PATH,
    fillRule: "nonzero",
  },
  github: { viewBox: "0 0 24 24", path: GITHUB_PATH },
  discord: { viewBox: "0 0 59 44", path: DISCORD_PATH },
};

/**
 * Official sign-in button chrome (fill / stroke / label) for the light SPA.
 *
 * Google: `#FFFFFF` fill, `#747775` 1px stroke, `#1F1F1F` label.
 * Apple: black button, white logo and title.
 * GitHub: Invertocat in white on `#1F2328`.
 * Discord: Clyde in white on Blurple `#5865F2`.
 */
const BUTTON_CLASS_LIGHT: Record<string, string> = {
  google:
    "h-10 border border-[#747775] bg-white font-google-sans font-medium text-[#1F1F1F] shadow-none hover:bg-[#F2F2F2]",
  apple:
    "h-10 border border-black bg-black font-medium text-white shadow-none hover:bg-[#1a1a1a]",
  github:
    "h-10 border border-[#1F2328] bg-[#1F2328] font-medium text-white shadow-none hover:bg-black",
  discord:
    "h-10 border border-[#5865F2] bg-[#5865F2] font-medium text-white shadow-none hover:bg-[#4752C4]",
};

/**
 * Official sign-in button chrome when the SPA has resolved to dark.
 *
 * Google: `#131314` fill, `#8E918F` stroke, `#E3E3E3` label (G stays the 2025
 * gradient Super G on a white tile). Apple: white fill, black logo and title. GitHub: black
 * Invertocat on white. Discord: white Clyde on black.
 */
const BUTTON_CLASS_DARK: Record<string, string> = {
  google:
    "h-10 border border-[#8E918F] bg-[#131314] font-google-sans font-medium text-[#E3E3E3] shadow-none hover:bg-[#1e1e1f]",
  apple:
    "h-10 border border-white bg-white font-medium text-black shadow-none hover:bg-[#f5f5f5]",
  github:
    "h-10 border border-white bg-white font-medium text-black shadow-none hover:bg-[#f5f5f5]",
  discord:
    "h-10 border border-black bg-black font-medium text-white shadow-none hover:bg-[#1a1a1a]",
};

/** Canonical social preset id, or `null` for a generic OpenID issuer. */
export function socialPresetId(preset: string | null | undefined): string | null {
  const key = (preset ?? "").trim().toLowerCase();
  return key in BRAND_NAMES ? key : null;
}

/**
 * Google/Apple require this exact “Continue with {Brand}” wording.
 *
 * @param preset - Built-in social preset, if any.
 * @param name - Operator-facing display name used for custom issuers.
 */
export function continueWithLabel(preset: string | null | undefined, name: string): string {
  const brand = socialPresetId(preset);
  return `Continue with ${brand ? BRAND_NAMES[brand] : name}`;
}

/**
 * Official vendor mark for Google, GitHub, Apple, or Discord sign-in UI.
 *
 * The full-color Google G must sit on white (Identity + Partner Marketing Hub);
 * a transparent G on parchment/navy is not allowed. Settings cards use a padded
 * chip per vendor: Google white, Apple/GitHub black-or-white, Discord Blurple
 * or black. Marks on Continue-with buttons inherit the button label color.
 *
 * @param props - Preset id, optional size class, and whether the mark sits on a branded button.
 */
export function SsoProviderMark({
  preset,
  className,
  title,
  onBrand = false,
}: {
  preset: string | null | undefined;
  className?: string;
  title?: string;
  /** When true, inherit the button label color (white or black per vendor chrome). */
  onBrand?: boolean;
}) {
  const dark = useResolvedDark();
  const brand = socialPresetId(preset);
  if (!brand) {
    return (
      <span
        className={cn(
          "flex h-6 w-6 shrink-0 items-center justify-center rounded-[2px] bg-ink/10 text-[10px] font-semibold uppercase text-ink/70",
          className,
        )}
        aria-hidden
      >
        {(title ?? "OIDC").slice(0, 2)}
      </span>
    );
  }

  if (brand === "google") {
    // Light SIWG: the button is already white, so the G sits on it directly.
    // Dark SIWG and settings cards: white tile — Identity + Partner Hub require
    // the full-color G on white (or black), never on navy/cream/blurple.
    if (onBrand && !dark) {
      return (
        <img
          src={googleMark}
          alt=""
          className={cn("h-6 w-6 shrink-0 object-contain", className)}
        />
      );
    }
    return (
      <span
        className={cn(
          "flex h-6 w-6 shrink-0 items-center justify-center rounded-[2px] bg-white",
          onBrand ? "p-0.5" : "p-1 ring-1 ring-ink/15",
          className,
        )}
      >
        <img src={googleMark} alt="" className="h-full w-full object-contain" />
      </span>
    );
  }

  const mark = MONO_MARKS[brand];
  if (!mark) {
    return null;
  }

  if (onBrand) {
    return (
      <svg
        viewBox={mark.viewBox}
        aria-hidden
        className={cn("h-6 w-6 shrink-0 text-current", className)}
      >
        <path fill="currentColor" fillRule={mark.fillRule} d={mark.path} />
      </svg>
    );
  }

  const chipTone =
    brand === "discord"
      ? dark
        ? "bg-black text-white"
        : "bg-[#5865F2] text-white"
      : brand === "github"
        ? dark
          ? "bg-white text-black"
          : "bg-[#1F2328] text-white"
        : dark
          ? "bg-white text-black"
          : "bg-black text-white";

  return (
    <span
      className={cn(
        "flex h-6 w-6 shrink-0 items-center justify-center rounded-[2px] p-1 ring-1 ring-ink/15",
        chipTone,
        className,
      )}
    >
      <svg viewBox={mark.viewBox} aria-hidden className="h-full w-full">
        <path fill="currentColor" fillRule={mark.fillRule} d={mark.path} />
      </svg>
    </span>
  );
}

/**
 * Sign-in / elevate control using official brand colors and “Continue with …” copy.
 *
 * Chrome follows the **resolved app theme**, not the OS hint alone.
 *
 * @param props - Preset, display name, and native button attributes.
 */
export function SsoSignInButton({
  preset,
  name,
  className,
  ...props
}: {
  preset: string | null | undefined;
  name: string;
} & ButtonHTMLAttributes<HTMLButtonElement>) {
  const brand = socialPresetId(preset);
  const dark = useResolvedDark();
  const chrome = dark ? BUTTON_CLASS_DARK : BUTTON_CLASS_LIGHT;
  return (
    <Button
      type="button"
      variant="secondary"
      className={cn(brand ? chrome[brand] : "h-10", className)}
      {...props}
    >
      <SsoProviderMark
        preset={preset}
        title={name}
        onBrand={Boolean(brand)}
        className="h-5 w-5"
      />
      {continueWithLabel(preset, name)}
    </Button>
  );
}
