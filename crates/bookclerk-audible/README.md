# bookclerk-audible

Thin wrapper over [audible-rs](https://github.com/mkb79/audible-rs) for Bookclerk-specific
auth, library sync, and download options.

## audible-rs pin

```
git = "https://github.com/mkb79/audible-rs"
rev = "5a28f507072022ae7fd7f95a62e3bdc5e515d678"
```

Auth files live under `{BOOKCLERK_FILES_DIR}/Accounts/<account>.auth` (audible-rs
envelope). Prefer encrypting at rest (Argon2id + XChaCha20-Poly1305). Widevine L3
CDMs live alongside them as `{BOOKCLERK_FILES_DIR}/Accounts/<account>.wvd`.

Passphrase sources (first match wins): `BOOKCLERK_AUTH_PASSWORD`,
`BOOKCLERK_AUTH_PASSWORD_FILE`, or `[auth].password_file`. A configured password
**file path that does not exist yet** is created with a strong random secret —
point it at a dedicated secrets volume, not `Accounts/`. Set
`auth.allow_plaintext = true` to store unprotected token files (local/dev).
`bookclerk-library` uses the same rusqlite 0.40 + bundled SQLite.

## Login modes

- **Server (default):** local reverse-proxy + terminal QR — best for SSH/Docker with port forward
- **External (`--external`):** print authorize URL; paste redirect on stdin or `--response-url`

Both modes open Amazon's OAuth / device-registration flow in a browser. There is
**no** username/password CLI flag: credentials are entered in the browser
(audible-rs). Amazon accounts with **2FA/MFA enabled require completing OTP**
(or SMS / mobile verification) during that browser step. Headless agents need
either an interactive Desktop session or a TOTP seed to finish login; importing
an existing `{BOOKCLERK_FILES_DIR}/Accounts/*.auth` file skips login entirely —
encrypted files need a matching passphrase; plaintext files need
`auth.allow_plaintext` only when *writing* unprotected envelopes.

## Acquire download path

`fetch_and_download_with_options`:

1. **Adrm** (default): `request_license` → `.aaxc` + voucher key/iv
2. On **000307** (no aaxc) or `widevine=true`: Widevine via native L3 CDM
   - Audible returns a DASH MPD → one CENC **fragmented MP4** (AAC-LC and/or xHE-AAC)
   - optional **xHE-AAC** codec preference (`xhe_aac=true`)
   - Mpeg fallback (plain mp3) when the server has no Widevine asset
3. Spatial/Atmos (Widevine **L1**, `ec+3`) is not available on desktop — acquire never requests it

CDM resolution: local `output.widevine_cdm` / `{files_dir}/widevine.wvd` /
`Accounts/<account>.wvd`, else auto-provision from
`output.widevine_cdm_provider` (default: classic Libation AudibleCdm).
Requires Android registration (`auth login` always uses Android).
