# bookclerk-plugin-source-audible

First-party Audible storefront plugin: a native guest binary the host runs
jailed, over [audible-rs](https://github.com/mkb79/audible-rs) for login,
library scan, catalog lookups and download + decrypt. The guest links
`bookclerk-storefront` and the plugin SDK, not the host's library, config or
media crates.

**Jail / network:** audible-rs opens HTTP sockets directly under coarse jail
outbound (`NetPolicy::Outbound` / `OutboundListen` with oauth). Native plugins
must **not** declare `capabilities.network.domains` (workerd-only hostname
allowlists). Prefer a workerd guest when you need enforceable domain filtering.

## audible-rs pin

```toml
git = "https://github.com/mkb79/audible-rs"
rev = "5a28f507072022ae7fd7f95a62e3bdc5e515d678"
```

The host keeps the Audible credentials (an audible-rs auth file, plus an
optional Widevine `.wvd`) sealed in the `encrypted_secrets` DB table with the
process DEK from `master.key` (XChaCha20-Poly1305), and passes them to the guest
on each call. The guest never opens the library database or `master.key`.
audible-rs is a library-only dependency here (`default-features = false`) and
does not link SQLite.

## Login

Accounts connect starts `loginStart`: the guest runs audible-rs's LoginServer
behind the host's callback tunnel and registers the device as Android (needed
for Widevine). There is **no** username/password flag: credentials are entered
in the browser. Amazon accounts with **2FA/MFA enabled require completing OTP**
(or SMS / mobile verification) during that browser step, so headless agents
need an interactive Desktop session or a TOTP seed.

## Acquire download path

`fetchTitle` (`guest_fetch_title`):

1. **Adrm** (default): `request_license` → `.aaxc` + voucher key/iv
2. On **000307** (no aaxc) or `widevine=true`: Widevine via native L3 CDM
   - Audible returns a DASH MPD → one CENC **fragmented MP4** (AAC-LC and/or xHE-AAC)
   - optional **xHE-AAC** codec preference (`xhe_aac=true`)
   - Mpeg fallback (plain mp3) when the server has no Widevine asset
3. Spatial/Atmos (Widevine **L1**, `ec+3`) is not available on desktop — acquire never requests it

CDM resolution: the `.wvd` the host passes in the credentials, or the BYO
`widevine_cdm` path the host grants. The guest does not provision a CDM from
the remote provider (`widevine_cdm_provider`) itself.
