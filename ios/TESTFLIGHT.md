# Diri iPhone beta release

This prepares a build, not a public launch. Start with a small internal test,
then invite nontechnical external testers after a real-device smoke test.
External testers install Diri through TestFlight, not Xcode.

## Owner setup (once)

- An active paid Apple Developer membership and an App Store Connect user with
  permission to upload builds. Add the account in Xcode → Settings → Accounts.
- Confirm the team and reserve the explicit App ID `com.cristicretu.diri.phone`.
  If that ID is unavailable, change `PRODUCT_BUNDLE_IDENTIFIER` in `project.yml`
  before the first distribution. Do not change it between beta builds.
- Create the iOS app record in App Store Connect with that same bundle ID.
- Choose a feedback email, beta contact details, and a hosted privacy-policy
  URL. These are owner-supplied values; this repository does not invent them.
- Review Apple's export-compliance questionnaire. The app uses Apple's
  URLSession/Keychain and a separately installed Tailscale app; it does not
  bundle WireGuard or its own cryptography. `ITSAppUsesNonExemptEncryption` is
  deliberately not asserted until the owner has confirmed the answers.

Do not commit certificates, provisioning profiles, `.p8` keys, or passwords.
Use Xcode-managed signing for the first beta; no CI signing secrets are needed.

## Build and validate

Use stable Xcode 26 or later with iOS SDK 26+ and XcodeGen. The minimum runtime
remains iOS 17; an SDK requirement is not a deployment-target requirement.

From `ios/`:

```sh
bash scripts/testflight.sh check
xcodebuild -project DiriPhone.xcodeproj -scheme DiriPhone \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  CODE_SIGNING_ALLOWED=NO test

DIRI_APPLE_TEAM_ID=YOURTEAMID DIRI_BUILD_NUMBER=2 \
  bash scripts/testflight.sh archive
```

Replace `YOURTEAMID` with the team's 10-character ID. Use an unused, increasing
build number for each upload. `MARKETING_VERSION` in `project.yml` controls the
visible version; the archive command overrides only the build number. Existing
archives and exports are never overwritten.

The archive command does not automatically register App IDs or download new
profiles. If you want Xcode to manage provisioning, explicitly add
`DIRI_ALLOW_PROVISIONING_UPDATES=1` to that invocation. This permits Apple account
changes and may require sign-in in Xcode. Simulator tests need no account.

Open `build/testflight/DiriPhone-2.xcarchive` in Xcode Organizer. Validate it,
inspect the privacy report and signing team, then choose **Distribute App →
App Store Connect** when ready to upload. Do not use the internal-only export
if this build will later go to external testers.

For a local IPA instead (no upload):

```sh
DIRI_APPLE_TEAM_ID=YOURTEAMID DIRI_BUILD_NUMBER=2 \
  bash scripts/testflight.sh export
```

`ExportOptions.plist` uses `app-store-connect` with `destination=export`.
Archives include symbols. No command here uploads a build, changes tester
groups, or submits a build for review.

## Privacy and review notes

`PrivacyInfo.xcprivacy` describes the current iPhone binary: no tracking,
developer-collected data, or required-reason API usage. Camera frames are
processed on-device and are not retained or uploaded. Pairing credentials are
stored in Keychain; session content goes to the user's chosen Diri gateway.
There are no analytics, advertising, crash-reporting SDKs or third-party
packages in this target. Re-audit this declaration if dependencies or APIs
change. Tailscale and Apple TestFlight have separate privacy practices.

Explain the ATS exception to review: the gateway uses HTTP inside Tailscale's
encrypted tunnel and can be addressed by a private IP. The iPhone adds bearer
authentication and rejects redirects. The Mac setup binds only to its
Tailscale address. This is not a public web service and no VPN entitlement is
requested by Diri itself. Do not claim that Apple has accepted this design
until review is complete.

External review needs a usable environment. Arrange a **dedicated disposable
Mac/project and restricted test network**, or coordinate an agreed review path
with Apple. Do not put a personal Mac's pairing key in review notes. Do not
turn on public access to get around the reviewer setup requirement. The app
has no offline demo mode today; plan reviewer access before submitting.

Suggested beta description:

> Run and monitor coding agents on your Mac from your iPhone. See which sessions
> need you, start work in a separate git worktree, send prompts, answer agent
> questions, follow output, and review tracked changes. Requires Diri running on
> a powered Mac and Tailscale connected on both devices.

Suggested “What to test”:

> Complete setup without using Terminal. Scan the pairing code, start a session
> in a separate worktree, and send a prompt. Switch from Wi-Fi to mobile data,
> lock and unlock your phone, then continue the same session. Report unclear
> steps or lost drafts. Remove prompts, code, and pairing keys from feedback
> screenshots. Push notifications are not available in this beta.

## Real-device gate before inviting nontechnical testers

- Install the signed build on a physical iPhone. Test camera allow, deny,
  Settings recovery, scan and paste; credentials must survive relaunch.
- Start from Tailscale missing/signed-out/disconnected on the Mac. Follow the
  guide using only app buttons; allow the iPhone VPN prompt and use the same
  account. Do not change exit nodes, Tailscale SSH or router settings.
- Test mismatched accounts, restricted network access, expired pairing codes,
  a sleeping Mac, and gateway disabled. Each must fail without losing drafts
  or claiming a successful connection.
- With Wi-Fi off, create local and preconfigured SSH sessions. Create a new
  worktree from `main` while the original checkout is on another branch.
- Send input, answer an agent permission question, inspect tracked changes,
  lock/unlock, and reconnect to the same live session. Never auto-retry an
  ambiguous start or input operation.
- Confirm display sleep is fine but lid closure/manual sleep disconnects.
  Turning phone access off must revoke open connections; restarting Diri
  requires enabling access again and re-pairing with its fresh code.
- Check the smallest supported phone, large accessibility text, VoiceOver,
  and Reduce Motion. Ask a new tester to complete setup without coaching.

In App Store Connect, supply beta information, assign the processed build to
an internal group, then submit an external group for beta review. Ordinary
testers should not be added as App Store Connect team members just to skip
review. Apple may review the first external build; availability is not instant.

## Sources (checked September 2026)

- [Apple SDK upload requirements](https://developer.apple.com/app-store/submitting/)
- [TestFlight workflow and review](https://developer.apple.com/help/app-store-connect/test-a-beta-version/testflight-overview/)
- [Upload builds](https://developer.apple.com/help/app-store-connect/manage-builds/upload-builds/)
- [Tailscale on iOS](https://tailscale.com/docs/install/ios)
- [Tailscale on macOS](https://tailscale.com/docs/install/mac)
