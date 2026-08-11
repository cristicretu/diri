# Official Homebrew cask submission

Diri is currently installed from the maintained
[`cristicretu/homebrew-diri`](https://github.com/cristicretu/homebrew-diri)
tap. This runbook records the remaining eligibility gate and the exact process
for moving the proven cask to
[`Homebrew/homebrew-cask`](https://github.com/Homebrew/homebrew-cask) without
changing the artifact users receive.

## Eligibility snapshot

Checked on August 12, 2026:

- The canonical repository was created on August 4, 2026, at 10:08:23 UTC.
- The repository has 254 stars, above Homebrew's 225-star threshold for a
  self-submission by the repository owner.
- Homebrew says a repository less than 30 days old is normally ineligible. The
  remaining normal gate therefore opens on **September 3, 2026 at 10:08:23
  UTC**.

Do not submit before that date merely because the notability threshold is met.
On the submission day, re-read Homebrew's current
[Package Acceptance Policy](https://docs.brew.sh/Package-Acceptance-Policy) and
[Acceptable Casks](https://docs.brew.sh/Acceptable-Casks); the policy, commands,
and thresholds can change.

Recheck the live repository values rather than relying on this snapshot:

```sh
gh repo view cristicretu/diri --json createdAt,stargazerCount
```

## Source of truth

Do not hand-reconstruct the official cask. Start from
[`Casks/diri.rb`](https://github.com/cristicretu/homebrew-diri/blob/main/Casks/diri.rb)
in the maintained tap. Every Diri release updates that cask only after
`diri/scripts/publish-homebrew-cask.sh` proves that its SHA-256 matches the
immutable GitHub Release DMG and reads the pushed cask back from the remote.

Before copying it, verify the current release and digest again:

```sh
version="$(gh release view --repo cristicretu/diri --json tagName --jq '.tagName | ltrimstr("v")')"
gh release view "v${version}" --repo cristicretu/diri \
  --json assets \
  --jq ".assets[] | select(.name == \"diri-${version}-universal.dmg\") | .digest"
gh api repos/cristicretu/homebrew-diri/contents/Casks/diri.rb \
  --jq '.content' | base64 --decode
```

The release digest must be `sha256:<64 lowercase hexadecimal digits>`, and the
same 64 digits must appear in the cask's `sha256` stanza. The URL must continue
to name the versioned GitHub Release DMG.

## Prepare the Homebrew contribution

Follow Homebrew's current
[adding-software guide](https://docs.brew.sh/Adding-Software-to-Homebrew) to fork
and check out `Homebrew/homebrew-cask` from its latest default branch. Search
open and closed pull requests and the cask tree for an existing `diri`
submission before creating another one.

Copy the maintained cask into Homebrew's token directory:

```sh
cp /path/to/homebrew-diri/Casks/diri.rb \
  /path/to/homebrew-cask/Casks/d/diri.rb
```

Review the copied file against the current Cask Cookbook. In particular, keep:

- the versioned, developer-published GitHub Release URL and exact checksum;
- `depends_on macos: :sequoia`, matching Diri's macOS 15 deployment target;
- `auto_updates true`, because Diri has a signed, user-approved in-app updater;
- the conservative `zap` list, which deliberately preserves daemon session
  state under `~/Library/Application Support/Dirijor`.

## Validate the candidate

Run Homebrew's current new-cask checks from the contribution checkout:

```sh
brew style --fix --cask diri
brew audit --new --cask diri
HOMEBREW_NO_INSTALL_FROM_API=1 brew install --cask diri
brew uninstall --cask diri
brew lgtm --online
```

Inspect the installed application as well as the exit codes. Confirm that it is
the universal, signed, notarized `diri.app`, launches on a clean current macOS
system, and uninstalls without deleting existing Dirijor session state.

Follow Homebrew's current
[pull-request guide](https://docs.brew.sh/How-To-Open-a-Homebrew-Pull-Request),
including its AI/LLM rules: disclose the tool and model used in the initial
upstream pull request, personally review all assisted content before requesting
review, and answer maintainer questions yourself without AI/LLM assistance.
Open the pull request with concise test results and no generated or unrelated
files. Meeting the published thresholds does not guarantee acceptance; respond
to Homebrew maintainer feedback in the upstream pull request.

## After acceptance

Only after the official cask is merged:

1. Change installation examples to `brew install --cask diri`.
2. Keep `cristicretu/homebrew-diri` available as a transition path rather than
   breaking existing installations immediately.
3. Update the release flow so official-cask updates follow Homebrew's supported
   contribution automation instead of pushing to the maintained tap directly.
4. Confirm `brew upgrade --cask diri` and Diri's in-app updater still agree on
   the latest version before closing the tracking issue.
