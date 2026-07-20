# libation-audible

Thin wrapper over [audible-rs](https://github.com/mkb79/audible-rs) for Libation-specific
auth, library sync, and download options.

## audible-rs pin

```
git = "https://github.com/mkb79/audible-rs"
rev = "5a28f507072022ae7fd7f95a62e3bdc5e515d678"
```

Auth files live under `{LIBATION_FILES_DIR}/auth/*.auth` (audible-rs envelope, plain by
default for headless). `libation-library` uses the same rusqlite 0.40 + bundled SQLite.

## Login modes

- **Server (default):** local reverse-proxy + terminal QR — best for SSH/Docker with port forward
- **External (`--external`):** print authorize URL; paste redirect on stdin or `--response-url`

Both modes open Amazon's OAuth / device-registration flow in a browser. There is
**no** username/password CLI flag: credentials are entered in the browser
(audible-rs). Amazon accounts with **2FA/MFA enabled require completing OTP**
(or SMS / mobile verification) during that browser step. Headless agents need
either an interactive Desktop session or a TOTP seed to finish login; importing
an existing plain `{LIBATION_FILES_DIR}/auth/*.auth` file skips login entirely.

## Liberate download path

`fetch_and_download_with_options`:

1. **Adrm** (default): `request_license` → `.aaxc` + voucher key/iv
2. On **000307** (no aaxc) or `widevine=true`: Widevine/CENC via native CDM (`.wvd`)
   - optional **xHE-AAC** codec preference (`xhe_aac=true`)
   - Mpeg fallback (plain mp3) when the server has no Widevine asset

CDM search: `download.widevine_cdm`, `{files_dir}/widevine.wvd`, `Accounts/<account>.wvd`.
