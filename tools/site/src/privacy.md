# Privacy

Last changed 19 November 2026

Bookclerk is free software you run on your own computer. This page explains what information reaches the Bookclerk project when you visit this website{{#demo}}, try the live demo{{/demo}} or choose to share problem reports or usage check-ins from the app, what we do with it, and how long we keep it. We do not sell or share it for advertising, and nothing here tracks you from one site to another.

## The short version

- This website sets no cookies. The documentation pages remember display preferences, such as the language of a code sample, in your browser only; nothing identifying is stored or sent.
- We count visits with Cloudflare Web Analytics, which uses no cookies and does not identify you.
{{#demo}}- The live demo uses cookies only to keep your own private copy running, sends us problem reports so we can fix its bugs, and deletes that copy when you leave.
{{/demo}}- The Bookclerk app sends us nothing unless you turn on problem reports or weekly usage check-ins.
- This website{{#demo}}, the demo{{/demo}} and the report collector run on Cloudflare; problem reports are stored with Backblaze.

## This website

When you open a page, your browser connects to Cloudflare, which hosts the site. Like any web host, Cloudflare receives your IP address, the page you asked for and what your browser says about itself, so it can send the page back and keep the site safe from attacks. We do not keep logs of these requests ourselves.

To see which pages people read and how fast they load, the site uses [Cloudflare Web Analytics](https://www.cloudflare.com/web-analytics/). Cloudflare adds a small script to each page that reports the page address, where you came from, your type of browser and device, your country and how long the page took to load. Cloudflare says it uses no cookies or browser storage for this and does not fingerprint visitors, so we see totals, not people.

The site loads its fonts and images from itself, and makes no requests to anyone else apart from the analytics script above. It has no forms, accounts, comments or advertising. The documentation pages keep a few display choices in your browser's storage, such as the language you picked for a code sample. These stay on your device, are never sent to us and you can clear them in your browser's settings.

## Cookies, and why there is no cookie banner

Consent banners are for cookies that track or profile you. Cookies that a service needs to do what you asked for, such as keeping you signed in, do not need your consent, but you should still be told about them. These are all the cookies Bookclerk's services set:

| Cookie | Where | Why | How long |
| --- | --- | --- | --- |
{{#demo}}| `__Host-demo` | Live demo | Links your browser to your private copy of Bookclerk. | Until your demo ends, at most 30 minutes |
| `bookclerk_portal_session` | Live demo | Keeps you signed in to your copy, the same way the app does on your own computer. | Until your demo ends. A copy left behind no longer works and expires within 12 hours |
{{/demo}}| `__Host-bc_oauth` | Diagnostics dashboard | Protects the GitHub sign-in step from forgery. | 10 minutes |
| `__Host-bc_session` | Diagnostics dashboard | Keeps a project member signed in. | 8 hours, or until sign-out |

None of them is used for analytics or advertising, and none is readable by other sites. This website itself sets none. Because every cookie is strictly needed for the service you chose to use, we do not show a consent banner.

Cloudflare may also set its own cookies on any of these sites when it needs to protect them from attacks or automated traffic, for example to remember that your browser passed a security check. Those are used only for that protection.
{{#demo}}
## The live demo

The [live demo]({{demoUrl}}) starts a private copy of Bookclerk just for you, on Cloudflare's servers. No account or email address is needed.

- **Analytics.** The demo's own pages, such as the start page, are counted with Cloudflare Web Analytics like this website. Your copy of the app is not: no analytics script is added to it.
- **Bot check.** Before your copy starts, [Cloudflare Turnstile](https://www.cloudflare.com/turnstile-privacy-policy/) checks that you are a person. It runs a few tests in your browser and sends the result to Cloudflare. It does not read what you type.
- **Limits.** To share the demo fairly, we count how many demos each visitor starts. We do not store your IP address: we store a scrambled code made from it that changes every day, so it cannot be turned back into your address or linked across days.
- **Your copy.** Your copy never learns your IP address. If you sign in to a store, your password or sign-in goes only to your copy and to that store. Your copy, your store sign-ins and anything you scanned are deleted when you press **Start over**, after 10 minutes without activity, or after 30 minutes. Before that, Bookclerk signs your store accounts out.
- **In your browser.** The app inside the demo remembers a few display choices in your browser's storage, such as light or dark mode and whether you closed the welcome message. These stay on your device and you can clear them in your browser's settings.
- **Problem reports.** Your copy sends problem reports, the same ones the app sends when you turn them on (described below), so we hear about bugs in the demo. They have your passwords, sign-in tokens, book titles and account IDs removed, and never your IP address.
- **What we keep.** For each demo we keep a record of when it started and ended, why it ended, how many store sign-ins were started and signed out, and the daily code above. The records name no one. We delete them after 30 days, and the daily counts after two days.
- **Usage figures.** So we can see how much the demo is used, each demo also sends our diagnostics dashboard when it started, how long it took to start, and when and why it ended, under a random number made for that demo alone. It does not include your IP address, the daily code or your cookie. We keep these figures for 400 days.
{{/demo}}
## Problem reports from the app

The Bookclerk app runs on your computer and talks only to the stores and services you connect it to. It does not send the Bookclerk project anything unless you turn on problem reports (`diagnostics.share_reports`), which is off unless you choose it.

With reports turned on, when Bookclerk crashes, a command or job fails, or many errors or warnings happen in a short time, it sends a report to our collector. A report holds:

- the Bookclerk version, your operating system, its processor type and Linux distribution, and the compiler version;
- the last few hundred lines of Bookclerk's own log, such as which store or job was running and what went wrong;
- the time it was made, and the name and version of the program that sent it.

Before a report leaves your computer, Bookclerk removes passwords, sign-in tokens and keys it knows about, and anything that looks like one. It also hides book titles, folder paths and account IDs, and partly masks email addresses. The collector refuses reports that still look like they contain a secret, and it does not store the IP address a report came from.

Reports are stored with [Backblaze B2](https://www.backblaze.com/company/policy/privacy), a storage service, and read by project members to find and fix bugs. Once a day a job on GitHub Actions collects new reports and asks an AI model on Cloudflare (Workers AI) to group them by problem. For each problem we open an issue on our public GitHub tracker. The issue describes the problem and may quote short parts of the log, with links and anything that looks like a secret removed, and it lists the reports' IDs. The full reports are never published.

Reports are deleted automatically 6 months after they arrive, or sooner when you ask. A one-line summary of each (the version, system and type of error, without the log) stays in the dashboard's counts.

Your Bookclerk keeps a list of the reports it sent on your own computer (in Settings, under Diagnostics), with each report's ID and a private access code. The code is given only to your Bookclerk, and we keep only a scrambled (hashed) form of it, so the IDs listed on our public tracker open nothing by themselves. With the code you can read a report back, see which issue it joined and whether it is fixed, and delete it, from Settings, from the command line, or on our report lookup page.

Deleting a report removes the stored report itself. Its one-line summary and its links to issues stay, so it still counts toward the problem it was part of.

You can turn reports off at any time. To have a report you sent deleted without its code, email us its report ID, or the time it was sent and your Bookclerk version.

## Usage check-ins from the app

To get a rough idea of how many people use Bookclerk, the app can check in with our collector once a week. This is a separate switch (`diagnostics.share_usage`), and it is off unless you choose it. A check-in holds only:

- a random ID your copy of Bookclerk makes for itself the first time, which says nothing about you or your computer;
- the Bookclerk version, your operating system and its processor type;
- whether it is the background service or the desktop app.

The collector does not keep that ID. It keeps a scrambled form of it (a hash) made with a secret that changes every month and is then deleted, so we can count how many copies checked in this week or this month, but cannot follow one copy from month to month. Check-ins are deleted after 35 days, and the collector does not store the IP address a check-in came from.

With check-ins turned on, problem reports carry the same ID, and the collector swaps it for the same monthly scrambled form before storing the report. That lets us see how many copies a problem affects. With check-ins off, reports carry no ID at all.

You can turn check-ins off at any time, and `bookclerk diagnostics reset-id` gives your copy a new ID.

## The diagnostics dashboard

The diagnostics dashboard is only for members of the Bookclerk organization on GitHub. Signing in with GitHub tells us your GitHub user name and ID and whether you are a member. We keep that with your session for 8 hours, and keep a log of sign-ins, of which reports were opened and of the triage changes members made for 90 days. We do not keep your GitHub token.

## Who else handles your information

- **Cloudflare** hosts this website{{#demo}}, the demo{{/demo}} and the report collector, runs the visit counter{{#demo}} and the demo's bot check{{/demo}}, and processes requests in many countries. See the [Cloudflare privacy policy](https://www.cloudflare.com/privacypolicy/).
- **Backblaze** stores problem reports.
- **GitHub** handles sign-in to the dashboard and hosts our code and public issues, and runs the job that groups problem reports. See the [GitHub privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).
- The **stores** you sign in to in the app{{#demo}} or the demo{{/demo}} see that sign-in under their own privacy policies. Bookclerk is not affiliated with them.

## Your choices

- Block or clear cookies and site data in your browser; this website works the same either way.
- Use a content blocker to stop the visit counter; nothing on the site depends on it.
- Leave problem reports and usage check-ins off, or turn them off again in Bookclerk's settings.
- Ask us what we hold about you, or to delete it, at the address below.

## Changes and contact

If what we collect changes, we update this page and the date at the top. The page's history is public in our [GitHub repository]({{repo}}). Questions or requests: [{{contactEmail}}](mailto:{{contactEmail}}).
