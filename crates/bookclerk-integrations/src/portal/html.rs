//! Minimal portal HTML with branded source / integration buttons.

use super::brands::Brand;

#[must_use]
pub fn landing_page(
    portal_base: &str,
    credential_providers: &[Brand],
    enabled_sources: &[Brand],
) -> String {
    let brands_json = brands_js_object(enabled_sources, credential_providers);
    let abs_section = if credential_providers.is_empty() {
        String::new()
    } else {
        let mut buttons = String::new();
        for brand in credential_providers {
            buttons.push_str(&format!(
                r#"
  <button type="button" class="brand-btn" data-provider="{id}"
    style="--brand-bg:{bg};--brand-fg:{fg};--brand-accent:{accent}">
    <img class="brand-logo" src="{logo}" alt="" width="28" height="28" loading="lazy" decoding="async">
    <span>Sign in with {name}</span>
  </button>
  <form class="cred-form" data-provider-form="{id}" hidden>
    <label>Username <input name="username" required autocomplete="username"></label>
    <label>Password <input name="password" type="password" required autocomplete="current-password"></label>
    <button type="submit" class="brand-btn compact"
      style="--brand-bg:{bg};--brand-fg:{fg};--brand-accent:{accent}">Continue with {name}</button>
  </form>
"#,
                id = brand.id,
                name = brand.name,
                bg = brand.bg,
                fg = brand.fg,
                accent = brand.accent,
                logo = brand.icon_url,
            ));
        }
        format!(
            r#"
<section class="card">
  <h2>Integration sign-in</h2>
  <p class="muted">Return later to manage or revoke store connections.</p>
  <div class="brand-stack">{buttons}</div>
</section>
"#
        )
    };

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Bookclerk Connect</title>
<style>
  :root {{ color-scheme: light dark; --bg: #12141a; --fg: #ece8e1; --muted: #9a9690; --accent: #3d8bfd; --card: #1c1f27; --line: #2c3140; }}
  @media (prefers-color-scheme: light) {{
    :root {{ --bg: #f3f1ec; --fg: #17181c; --muted: #5c5852; --card: #fff; --line: #ddd8cf; }}
  }}
  body {{ font-family: "Iowan Old Style", "Palatino Linotype", Palatino, Georgia, serif;
    margin: 0; background:
      radial-gradient(900px 480px at 8% -12%, #243044 0%, transparent 55%),
      radial-gradient(700px 420px at 100% 0%, #1a2a24 0%, transparent 50%),
      var(--bg);
    color: var(--fg); min-height: 100vh; }}
  main {{ max-width: 36rem; margin: 0 auto; padding: 2.5rem 1.25rem; }}
  h1 {{ font-size: 1.85rem; letter-spacing: -0.02em; margin: 0 0 0.35rem; }}
  h2 {{ font-size: 1.15rem; margin: 0 0 0.5rem; }}
  .lead, .muted {{ color: var(--muted); margin: 0 0 1.25rem; }}
  .card {{ background: var(--card); border-radius: 14px; padding: 1.25rem 1.35rem; margin-bottom: 1rem;
    border: 1px solid color-mix(in srgb, var(--line) 80%, transparent);
    box-shadow: 0 10px 36px rgba(0,0,0,.16); }}
  label {{ display: block; margin: 0.65rem 0; font-size: 0.95rem; }}
  input {{ width: 100%; box-sizing: border-box; margin-top: 0.25rem; padding: 0.55rem 0.65rem;
    border-radius: 8px; border: 1px solid var(--line); background: transparent; color: inherit; }}
  button, .brand-btn {{ font: inherit; cursor: pointer; }}
  button.plain {{ margin-top: 0.75rem; padding: 0.55rem 1rem; border: 0; border-radius: 8px;
    background: var(--accent); color: #fff; }}
  .brand-stack {{ display: grid; gap: 0.65rem; }}
  .brand-grid {{ display: grid; gap: 0.75rem; }}
  .brand-btn {{
    display: inline-flex; align-items: center; gap: 0.7rem; width: 100%;
    padding: 0.7rem 0.9rem; border-radius: 10px; border: 2px solid var(--brand-accent, #555);
    background: var(--brand-bg, #333); color: var(--brand-fg, #fff);
    font-weight: 600; letter-spacing: 0.01em; text-align: left;
    transition: transform .12s ease, filter .12s ease, box-shadow .12s ease;
  }}
  .brand-btn:hover {{ transform: translateY(-1px); filter: brightness(1.05);
    box-shadow: 0 8px 20px color-mix(in srgb, var(--brand-bg) 35%, transparent); }}
  .brand-btn:focus-visible {{ outline: 3px solid var(--brand-accent); outline-offset: 2px; }}
  .brand-btn.compact {{ width: auto; margin-top: 0.5rem; }}
  .brand-logo {{ width: 28px; height: 28px; border-radius: 6px; flex: 0 0 auto;
    object-fit: contain; background: #fff; padding: 2px; box-sizing: border-box; }}
  .brand-logo.sm {{ width: 22px; height: 22px; border-radius: 4px; padding: 1px; }}
  .source-panel {{ margin-top: 0.65rem; padding-top: 0.65rem; border-top: 1px dashed var(--line); }}
  .err {{ color: #ff8e8e; margin-top: 0.75rem; white-space: pre-wrap; }}
  a {{ color: var(--accent); }}
  #app[hidden], [hidden] {{ display: none !important; }}
  ul.connections {{ list-style: none; padding: 0; margin: 0; }}
  ul.connections li {{
    display: flex; align-items: center; gap: 0.55rem; flex-wrap: wrap;
    padding: 0.55rem 0; border-bottom: 1px solid var(--line);
  }}
  ul.connections li:last-child {{ border-bottom: 0; }}
  .conn-meta {{ flex: 1 1 auto; min-width: 10rem; }}
  .chip {{
    display: inline-flex; align-items: center; gap: 0.35rem;
    padding: 0.15rem 0.45rem; border-radius: 999px; font-size: 0.8rem;
    background: color-mix(in srgb, var(--brand-bg, #444) 22%, transparent);
    border: 1px solid color-mix(in srgb, var(--brand-accent, #666) 55%, transparent);
  }}
  .status {{ color: var(--muted); font-size: 0.85rem; }}
</style>
</head>
<body>
<main>
  <h1>Bookclerk Connect</h1>
  <p class="lead">Link bookstore accounts. Acquired books stay when you revoke.</p>

  <div id="gate">
    <section class="card">
      <h2>Claim ticket</h2>
      <p class="muted">Use a ticket issued when your library user was created.</p>
      <form id="ticket-form">
        <label>Ticket <input name="ticket" required autocomplete="off" spellcheck="false"></label>
        <button class="plain" type="submit">Continue</button>
      </form>
    </section>
    {abs_section}
    <p class="err" id="gate-err" hidden></p>
  </div>

  <div id="app" hidden>
    <section class="card">
      <h2>Signed in</h2>
      <p id="who" class="muted"></p>
      <button class="plain" type="button" id="logout">Sign out</button>
    </section>
    <section class="card">
      <h2>Bookstore sources</h2>
      <p class="muted">Choose a store to connect.</p>
      <div id="sources" class="brand-grid"></div>
    </section>
    <section class="card">
      <h2>Connections</h2>
      <ul id="connections" class="connections"></ul>
    </section>
    <p class="err" id="app-err" hidden></p>
  </div>
</main>
<script>
const BASE = {base_json};
const BRANDS = {brands_json};
function brandOf(id) {{
  return BRANDS[id] || {{ bg:'#334155', fg:'#f8fafc', accent:'#64748b', name: id, icon: '' }};
}}
function logoUrl(id, apiLogo) {{
  if (apiLogo) return apiLogo;
  const b = brandOf(id);
  return b.icon || '';
}}
function api(path, opts={{}}) {{
  return fetch(BASE + path, Object.assign({{ credentials: 'same-origin', headers: {{ 'Content-Type': 'application/json' }} }}, opts))
    .then(async r => {{
      const text = await r.text();
      let data = null;
      try {{ data = text ? JSON.parse(text) : null; }} catch {{}}
      if (!r.ok) throw new Error((data && data.error) || text || r.statusText);
      return data;
    }});
}}
function showErr(el, msg) {{ el.hidden = !msg; el.textContent = msg || ''; }}
function brandButton(id, label, attrs, apiLogo) {{
  const b = brandOf(id);
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'brand-btn';
  btn.style.setProperty('--brand-bg', b.bg);
  btn.style.setProperty('--brand-fg', b.fg);
  btn.style.setProperty('--brand-accent', b.accent);
  if (attrs) Object.entries(attrs).forEach(([k,v]) => btn.setAttribute(k, v));
  const img = document.createElement('img');
  img.className = 'brand-logo';
  img.alt = '';
  img.width = 28; img.height = 28;
  img.loading = 'lazy';
  img.decoding = 'async';
  img.src = logoUrl(id, apiLogo);
  const span = document.createElement('span');
  span.textContent = label || ('Connect ' + (b.name || id));
  btn.appendChild(img);
  btn.appendChild(span);
  return btn;
}}
async function enterApp() {{
  document.getElementById('gate').hidden = true;
  document.getElementById('app').hidden = false;
  const me = await api('/api/me');
  const pb = brandOf(me.provider);
  const who = document.getElementById('who');
  who.textContent = '';
  const chip = document.createElement('span');
  chip.className = 'chip';
  chip.style.setProperty('--brand-bg', pb.bg);
  chip.style.setProperty('--brand-accent', pb.accent);
  const img = document.createElement('img');
  img.className = 'brand-logo sm';
  img.alt = '';
  img.src = logoUrl(me.provider);
  chip.appendChild(img);
  chip.appendChild(document.createTextNode(pb.name || me.provider));
  who.appendChild(chip);
  who.appendChild(document.createTextNode(' ' + (me.label || me.external_user_id)));
  await refreshSources();
  await refreshConnections();
}}
async function refreshSources() {{
  const data = await api('/api/sources');
  const root = document.getElementById('sources');
  root.innerHTML = '';
  for (const s of data.sources || []) {{
    const wrap = document.createElement('div');
    const id = s.id;
    const name = s.name || brandOf(id).name || id;
    const auth = s.auth || (id === 'audible' ? 'oauth' : 'password');
    const apiLogo = s.brand && s.brand.logo;
    const btn = brandButton(id, 'Connect ' + name, {{ 'data-source': id }}, apiLogo);
    wrap.appendChild(btn);
    const panel = document.createElement('div');
    panel.className = 'source-panel';
    panel.hidden = true;
    if (auth === 'password') {{
      panel.innerHTML = '<form data-password>' +
        '<label>Email <input name="email" type="email" required autocomplete="username"></label>' +
        '<label>Password <input name="password" type="password" required autocomplete="current-password"></label>' +
        '<button type="submit" class="brand-btn compact">Save ' + name + ' login</button></form>';
      const b = brandOf(id);
      const submit = panel.querySelector('button');
      submit.style.setProperty('--brand-bg', b.bg);
      submit.style.setProperty('--brand-fg', b.fg);
      submit.style.setProperty('--brand-accent', b.accent);
      panel.querySelector('form').addEventListener('submit', async (e) => {{
        e.preventDefault();
        showErr(document.getElementById('app-err'), '');
        const fd = new FormData(e.target);
        try {{
          await api('/api/sources/' + encodeURIComponent(id) + '/login', {{
            method: 'POST',
            body: JSON.stringify({{ email: fd.get('email'), password: fd.get('password') }})
          }});
          panel.hidden = true;
          await refreshConnections();
        }} catch (err) {{ showErr(document.getElementById('app-err'), err.message); }}
      }});
    }} else {{
      panel.innerHTML = '<p class="muted">Opens the store sign-in flow in a new tab.</p>' +
        '<a href="#" target="_blank" rel="noopener" hidden>Open ' + name + ' login</a>';
    }}
    btn.addEventListener('click', async () => {{
      showErr(document.getElementById('app-err'), '');
      if (auth === 'password') {{
        panel.hidden = !panel.hidden;
        return;
      }}
      try {{
        const res = await api('/api/sources/' + encodeURIComponent(id) + '/oauth/start', {{
          method: 'POST', body: '{{}}'
        }});
        panel.hidden = false;
        const a = panel.querySelector('a');
        a.href = res.url;
        a.hidden = false;
        a.textContent = 'Open ' + name + ' login';
        window.open(res.url, '_blank', 'noopener');
      }} catch (err) {{
        try {{
          const res = await api('/api/audible/start', {{ method: 'POST', body: '{{}}' }});
          panel.hidden = false;
          const a = panel.querySelector('a');
          a.href = res.url;
          a.hidden = false;
          a.textContent = 'Open Audible login';
          window.open(res.url, '_blank', 'noopener');
        }} catch (err2) {{
          showErr(document.getElementById('app-err'), err.message || err2.message);
        }}
      }}
    }});
    wrap.appendChild(panel);
    root.appendChild(wrap);
  }}
}}
async function refreshConnections() {{
  const data = await api('/api/connections');
  const ul = document.getElementById('connections');
  ul.innerHTML = '';
  for (const c of data.connections || []) {{
    const li = document.createElement('li');
    const b = brandOf(c.source);
    // Prefer API brand (present even when the source plugin is disabled and
    // omitted from the landing BRANDS map).
    const bg = (c.brand && c.brand.bg) || b.bg;
    const accent = (c.brand && c.brand.accent) || b.accent;
    const logo = (c.brand && c.brand.logo) || logoUrl(c.source);
    const chip = document.createElement('span');
    chip.className = 'chip';
    chip.style.setProperty('--brand-bg', bg);
    chip.style.setProperty('--brand-accent', accent);
    const img = document.createElement('img');
    img.className = 'brand-logo sm';
    img.alt = '';
    img.src = logo;
    chip.appendChild(img);
    chip.appendChild(document.createTextNode(b.name || c.source));
    const meta = document.createElement('span');
    meta.className = 'conn-meta';
    meta.appendChild(document.createTextNode(c.label || c.account_id));
    meta.appendChild(document.createTextNode(' '));
    const status = document.createElement('span');
    status.className = 'status';
    status.textContent = '[' + (c.connection_status || 'active') + ']';
    meta.appendChild(status);
    if (c.source_enabled === false) {{
      meta.appendChild(document.createTextNode(' '));
      const disabled = document.createElement('span');
      disabled.className = 'muted';
      disabled.textContent = '(source disabled)';
      meta.appendChild(disabled);
    }}
    li.appendChild(chip);
    li.appendChild(meta);
    // Revoke remains available even when the source plugin is disabled.
    if (c.connection_status !== 'revoked') {{
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'plain';
      btn.textContent = 'Revoke';
      btn.addEventListener('click', async () => {{
        if (!confirm('Revoke store credentials? Acquired books are kept.')) return;
        await api('/api/connections/' + encodeURIComponent(c.account_id) + '/revoke', {{ method: 'POST', body: '{{}}' }});
        await refreshConnections();
      }});
      li.appendChild(btn);
    }}
    ul.appendChild(li);
  }}
  if (!(data.connections || []).length) {{
    ul.innerHTML = '<li class="muted">No store connections yet.</li>';
  }}
}}
document.getElementById('ticket-form').addEventListener('submit', async (e) => {{
  e.preventDefault();
  showErr(document.getElementById('gate-err'), '');
  const ticket = new FormData(e.target).get('ticket');
  try {{
    await api('/api/redeem', {{ method: 'POST', body: JSON.stringify({{ ticket }}) }});
    await enterApp();
  }} catch (err) {{ showErr(document.getElementById('gate-err'), err.message); }}
}});
document.querySelectorAll('[data-provider]').forEach((btn) => {{
  btn.addEventListener('click', () => {{
    const id = btn.getAttribute('data-provider');
    document.querySelectorAll('[data-provider-form]').forEach((f) => {{
      const match = f.getAttribute('data-provider-form') === id;
      f.hidden = match ? !f.hidden : true;
    }});
  }});
}});
document.querySelectorAll('[data-provider-form]').forEach((form) => {{
  form.addEventListener('submit', async (e) => {{
    e.preventDefault();
    showErr(document.getElementById('gate-err'), '');
    const fd = new FormData(form);
    try {{
      await api('/api/login/integration', {{ method: 'POST', body: JSON.stringify({{
        provider: form.getAttribute('data-provider-form'),
        username: fd.get('username'),
        password: fd.get('password')
      }})}});
      await enterApp();
    }} catch (err) {{ showErr(document.getElementById('gate-err'), err.message); }}
  }});
}});
document.getElementById('logout').addEventListener('click', async () => {{
  await api('/api/logout', {{ method: 'POST', body: '{{}}' }});
  location.reload();
}});
const params = new URLSearchParams(location.search);
if (params.get('ticket')) {{
  document.querySelector('#ticket-form [name=ticket]').value = params.get('ticket');
}}
api('/api/me').then(() => enterApp()).catch(() => {{}});
</script>
</body>
</html>
"##,
        abs_section = abs_section,
        base_json = serde_json::to_string(portal_base).unwrap_or_else(|_| "\"\"".into()),
        brands_json = brands_json,
    )
}

/// Brand metadata embedded in the portal page — only for enabled plugins.
fn brands_js_object(enabled_sources: &[Brand], credential_providers: &[Brand]) -> String {
    let mut map = serde_json::Map::new();
    for brand in enabled_sources {
        map.insert(brand.id.to_string(), brand_json(brand));
    }
    for brand in credential_providers {
        map.insert(brand.id.to_string(), brand_json(brand));
    }
    serde_json::Value::Object(map).to_string()
}

fn brand_json(b: &Brand) -> serde_json::Value {
    serde_json::json!({
        "bg": b.bg,
        "fg": b.fg,
        "accent": b.accent,
        "name": b.name,
        "icon": b.icon_url,
    })
}

/// Brands that may appear on the gate for credential login (static id lookup).
///
/// Prefer [`crate::Integration::portal_brand`] from a live registry in
/// production; this helper exists for tests and offline HTML rendering.
#[cfg(test)]
#[must_use]
pub fn credential_login_brands(provider_ids: &[String]) -> Vec<Brand> {
    provider_ids
        .iter()
        .filter_map(|id| super::brands::integration_brand(id))
        .collect()
}
