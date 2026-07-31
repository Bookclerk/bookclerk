# bookclerk-plugin-source-audible

First-party Audible source plugin: in-process [`ContentSource`] via
`bookclerk_plugin_source_audible::register`, plus a JSON-RPC guest binary for staged /
sandboxed loads.

Thin wrapper over [audible-rs](https://github.com/mkb79/audible-rs) for Bookclerk-specific
auth, library sync, and download options.

## audible-rs pin

```
git = "https://github.com/mkb79/audible-rs"
rev = "5a28f507072022ae7fd7f95a62e3bdc5e515d678"
```

Audible OAuth tokens live in the `encrypted_secrets` DB table (sealed-v1
format), alongside Libro.fm / Chirp / GraphicAudio credentials. All credentials
are encrypted at rest using the process DEK from `master.key`
(XChaCha20-Poly1305). Widevine L3 CDMs are also stored in `encrypted_secrets`
(per account). Classic Libation `import libation` brings over account/library
metadata only — IdentityTokens are not converted; use `auth login` or
`auth import` of an audible-rs auth file.

Passphrase: set `BOOKCLERK_AUTH_PASSWORD` (env-only) to wrap `master.key` at
rest (strongly recommended for production). `bookclerk-library` uses the same
rusqlite 0.40 + bundled SQLite.

## Login modes

- **Server (default):** local reverse-proxy + terminal QR — best for SSH/Docker with port forward
- **External (`--external`):** print authorize URL; paste redirect on stdin or `--response-url`

Both modes open Amazon's OAuth / device-registration flow in a browser. There is
**no** username/password CLI flag: credentials are entered in the browser
(audible-rs). Amazon accounts with **2FA/MFA enabled require completing OTP**
(or SMS / mobile verification) during that browser step. Headless agents need
either an interactive Desktop session or a TOTP seed to finish login; importing
an existing audible-rs `*.audible.auth` file (`bookclerk auth import`) stores it
in `encrypted_secrets` and skips login entirely.

## Acquire download path

`fetch_and_download_with_options`:

1. **Adrm** (default): `request_license` → `.aaxc` + voucher key/iv
2. On **000307** (no aaxc) or `widevine=true`: Widevine via native L3 CDM
   - Audible returns a DASH MPD → one CENC **fragmented MP4** (AAC-LC and/or xHE-AAC)
   - optional **xHE-AAC** codec preference (`xhe_aac=true`)
   - Mpeg fallback (plain mp3) when the server has no Widevine asset
3. Spatial/Atmos (Widevine **L1**, `ec+3`) is not available on desktop — acquire never requests it

CDM resolution: `encrypted_secrets` (per account) → optional BYO file
`output.widevine_cdm` / `{files_dir}/widevine.wvd` (imported into the DB on
first use) → auto-provision from `output.widevine_cdm_provider` (default: classic
Libation AudibleCdm), stored back into `encrypted_secrets`. Requires Android
registration (`auth login` always uses Android).
