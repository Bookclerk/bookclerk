# CLA signatures

This branch records who has accepted the repository's Contributor License
Agreement (`CLA.md` on the default branch). It holds only this README, the
setup script beside it and `signatures/cla.json`, and shares no history
with the default branch.

Nobody edits it by hand. The CLA check writes it:

- The check runs on every pull request. It passes when the pull request
  author, every account linked to a commit and every linked
  `Co-authored-by:` account has signed. Bots and the allowlisted coding
  assistant accounts are exempt.
- A contributor signs by commenting the sentence the check's comment asks
  for on a pull request they are part of. The check then adds one entry to
  `signatures/cla.json` in one commit.
- The first signature started this branch with a commit that holds these
  files and an empty `signatures/cla.json`, then added itself in the next
  commit, so no signer's name is attached to the files here. Each commit
  is made through the GitHub API with a GitHub App's token, so GitHub signs
  it for the App.

Each entry records the GitHub account (`name`, `id`), the signing comment
(`comment_id`, `created_at`), where it was made (`repoId`,
`pullRequestNo`) and a permalink to the CLA text it accepts (`claUrl`,
`CLA.md` at the last default-branch commit that changed it).

## Set up the check in a repository

1. Copy these files from the default branch of the repository this branch
   came from:
   - `.github/workflows/cla-trigger.yml` and `.github/workflows/cla.yml`
   - `scripts/cla_check.py` and the `scripts/cla_branch/` folder (this
     README and `setup-rulesets.sh`, which the check copies here)
   - `CLA.md`, your agreement

   Then change the project-specific values at the top of
   `scripts/cla_check.py`: the signing sentence, the CLA version and the
   allowlisted bots and assistant accounts. The job's `if:` in `cla.yml`
   only starts on comments that contain the start of the signing sentence,
   so change that text to match, or signature comments will not run the
   check.
2. Create a GitHub App (**Settings → Developer settings → GitHub Apps**,
   for a user or an organization), upload a logo under **Display
   information** (GitHub shows it beside the check), and give it three
   repository permissions:
   - **Contents: read and write**, to create this branch and add
     signatures.
   - **Pull requests: read and write**, to post the check's comment under
     the App's name and avatar.
   - **Checks: read and write**, to post the `Bookclerk CLA` check run as
     the App, listed under its name and logo.

   It needs no webhook. Install it on the repository, then generate a
   private key.
3. Add two Actions secrets to the repository (**Settings → Secrets and
   variables → Actions**):
   - `AUTOMATION_APP_CLIENT_ID`: the App's client ID.
   - `AUTOMATION_APP_PRIVATE_KEY`: the private key.

   The workflow mints a short-lived token from them, narrowed to those
   permissions on this repository. Without them, the check still decides
   and reports as `github-actions[bot]`, but it cannot record a signature
   and reports an error instead.
4. Protect this branch. As a repository administrator, with the GitHub CLI
   signed in as yourself, run:

   ```bash
   bash setup-rulesets.sh OWNER/REPO APP_SLUG
   ```

   `APP_SLUG` is the App's name as it appears in `github.com/apps/<slug>`.
   The script creates two rulesets, or updates them when they exist:
   - **CLA signatures: writers** restricts creating and updating the
     branch, with only the App on its bypass list.
   - **CLA signatures: history** blocks deleting the branch, force pushes,
     merge commits and unsigned commits, with an empty bypass list, so it
     binds the App too.

   The App's commits meet the second ruleset, because each is one
   GitHub-signed commit on top of the branch. The App never needs the
   Administration permission. Every check run reads the rules that apply
   to this branch and warns when one is missing.
5. After the check has run once, require the **Bookclerk CLA** check in the default
   branch's ruleset, with the App as its expected source.

## Start over

Disable the **CLA signatures: history** ruleset, delete this branch and
enable the ruleset again. The next signature creates the branch afresh, and
every contributor signs again.
